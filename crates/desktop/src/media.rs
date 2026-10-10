//! Event-driven Windows media transport. WinRT and blocking async results stay off the UI/runtime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use audiobridge_core::session::{MediaCommand, PcMedia, Playback};
use windows::ApplicationModel::AppInfo;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
};

enum Message {
    Command(MediaCommand),
    Changed,
    Shutdown,
}

#[derive(Clone)]
pub struct Sender(mpsc::Sender<Message>);

impl Sender {
    pub fn send(&self, command: MediaCommand) {
        if self.0.send(Message::Command(command)).is_err() {
            tracing::warn!(?command, "media transport thread unavailable");
        }
    }
}

pub struct MediaTransport {
    sender: Sender,
    thread: JoinHandle<()>,
}

impl MediaTransport {
    pub fn spawn(report: impl Fn(PcMedia) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let changed = Changes {
            tx: tx.clone(),
            pending: Arc::new(AtomicBool::new(false)),
        };
        let thread = thread::Builder::new()
            .name("media-transport".into())
            .spawn(move || run(rx, changed, report))
            .context("spawn media transport thread")?;
        Ok(Self {
            sender: Sender(tx),
            thread,
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub fn shutdown(self) {
        let _ = self.sender.0.send(Message::Shutdown);
        if self.thread.join().is_err() {
            tracing::warn!("media transport thread failed during shutdown");
        }
    }
}

struct Apartment;

impl Apartment {
    fn new() -> Result<Self> {
        // SAFETY: this dedicated thread has no prior apartment; Drop balances successful initialization.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("initialize media WinRT MTA")?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: runs on the same thread as the successful RoInitialize, after all WinRT objects drop.
        unsafe { RoUninitialize() };
    }
}

#[derive(Clone)]
struct Changes {
    tx: mpsc::Sender<Message>,
    pending: Arc<AtomicBool>,
}

impl Changes {
    fn notify(&self) {
        // At most one outstanding event wakeup, without dropping or coalescing commands.
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Message::Changed);
        }
    }
}

struct ManagerWatch {
    manager: Manager,
    current: Option<i64>,
    sessions: Option<i64>,
}

impl ManagerWatch {
    fn new(changes: &Changes) -> Result<Self> {
        let manager = Manager::RequestAsync()
            .context("request GSMTC manager")?
            .join()
            .context("open GSMTC manager")?;
        let mut watch = Self {
            manager,
            current: None,
            sessions: None,
        };
        let changed = changes.clone();
        watch.current = Some(
            watch
                .manager
                .CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                    changed.notify();
                    Ok(())
                }))
                .context("subscribe to current media session")?,
        );
        let changed = changes.clone();
        watch.sessions = Some(
            watch
                .manager
                .SessionsChanged(&TypedEventHandler::new(move |_, _| {
                    changed.notify();
                    Ok(())
                }))
                .context("subscribe to media sessions")?,
        );
        Ok(watch)
    }
}

impl Drop for ManagerWatch {
    fn drop(&mut self) {
        if let Some(token) = self.current {
            let _ = self.manager.RemoveCurrentSessionChanged(token);
        }
        if let Some(token) = self.sessions {
            let _ = self.manager.RemoveSessionsChanged(token);
        }
    }
}

struct SessionWatch {
    session: Session,
    playback: Option<i64>,
    properties: Option<i64>,
}

impl SessionWatch {
    fn update(&mut self, reported: bool, changes: &Changes) -> Result<()> {
        // Watch playback for every session: a background player becoming Playing can change selection.
        if self.playback.is_none() {
            let changed = changes.clone();
            self.playback = Some(
                self.session
                    .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                        changed.notify();
                        Ok(())
                    }))
                    .context("subscribe to media playback")?,
            );
        }
        if reported && self.properties.is_none() {
            let changed = changes.clone();
            self.properties = Some(
                self.session
                    .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                        changed.notify();
                        Ok(())
                    }))
                    .context("subscribe to media properties")?,
            );
        } else if !reported {
            if let Some(token) = self.properties {
                self.session
                    .RemoveMediaPropertiesChanged(token)
                    .context("unsubscribe media properties")?;
                self.properties = None;
            }
        }
        Ok(())
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        if let Some(token) = self.playback {
            let _ = self.session.RemovePlaybackInfoChanged(token);
        }
        if let Some(token) = self.properties {
            let _ = self.session.RemoveMediaPropertiesChanged(token);
        }
    }
}

fn run(rx: mpsc::Receiver<Message>, changes: Changes, report: impl Fn(PcMedia)) {
    let apartment = Apartment::new();
    let manager = match apartment.as_ref() {
        Ok(_) => ManagerWatch::new(&changes),
        Err(error) => Err(anyhow::anyhow!("{error:#}")),
    };
    if let Err(error) = &manager {
        tracing::warn!("GSMTC unavailable; hardware media keys remain available: {error:#}");
    }
    let mut watches = Vec::new();
    refresh(manager.as_ref().ok(), &mut watches, &changes, &report);
    while let Ok(message) = rx.recv() {
        match message {
            Message::Shutdown => break,
            Message::Changed => {
                changes.pending.store(false, Ordering::Release);
                refresh(manager.as_ref().ok(), &mut watches, &changes, &report);
            }
            Message::Command(command) => {
                let selected = manager
                    .as_ref()
                    .ok()
                    .map(|m| selected_session(&m.manager))
                    .transpose();
                let mut aumid = String::new();
                let attempt = match selected {
                    Ok(Some(Some(session))) => {
                        aumid = session
                            .SourceAppUserModelId()
                            .map(|id| id.to_string())
                            .unwrap_or_default();
                        apply(&session, command).map(Some)
                    }
                    Ok(_) => Ok(None),
                    Err(error) => Err(error),
                };
                let gsmtc_error = attempt.as_ref().err().map(|e| format!("{e:#}"));
                match finish_command(attempt, || send_key(command)) {
                    Ok(route) => {
                        let fallback_toggle = route == Route::HardwareKeyFallback
                            && matches!(
                                command,
                                MediaCommand::Play | MediaCommand::Pause | MediaCommand::PlayPause
                            );
                        tracing::info!(?command, %aumid, ?route, ?gsmtc_error, fallback_toggle, "media command");
                    }
                    Err(error) => {
                        tracing::info!(?command, %aumid, ?gsmtc_error, "media command hardware-key fallback failed: {error:#}")
                    }
                }
                refresh(manager.as_ref().ok(), &mut watches, &changes, &report);
            }
        }
    }
    // Subscriptions and their callbacks must be released before the apartment.
    drop(watches);
    drop(manager);
    drop(apartment);
}

#[derive(Debug, PartialEq, Eq)]
enum Selection {
    Current,
    Listed(usize),
}

fn choose_session(
    current: Option<Playback>,
    mut sessions: impl Iterator<Item = Playback>,
) -> Option<Selection> {
    if current == Some(Playback::Playing) {
        Some(Selection::Current)
    } else if let Some(index) = sessions.position(|status| status == Playback::Playing) {
        Some(Selection::Listed(index))
    } else {
        current.map(|_| Selection::Current)
    }
}

fn playback(session: &Session) -> Result<Playback> {
    Ok(map_status(
        session
            .GetPlaybackInfo()
            .context("get playback info")?
            .PlaybackStatus()
            .context("get playback status")?,
    ))
}

fn sessions(manager: &Manager) -> Result<(Option<Session>, Vec<Session>)> {
    // GetCurrentSession returns E_POINTER for a null session when no app is current.
    let current = manager.GetCurrentSession().ok();
    let sessions = manager.GetSessions().context("enumerate media sessions")?;
    let mut list = Vec::with_capacity(sessions.Size().context("count media sessions")? as usize);
    for session in sessions {
        list.push(session);
    }
    Ok((current, list))
}

fn select<'a>(current: Option<&'a Session>, list: &'a [Session]) -> Option<&'a Session> {
    match choose_session(
        current.map(|s| playback(s).unwrap_or(Playback::Stopped)),
        list.iter()
            .map(|s| playback(s).unwrap_or(Playback::Stopped)),
    ) {
        Some(Selection::Current) => current,
        Some(Selection::Listed(index)) => list.get(index),
        None => None,
    }
}

fn selected_session(manager: &Manager) -> Result<Option<Session>> {
    let (current, list) = sessions(manager)?;
    Ok(select(current.as_ref(), &list).cloned())
}

fn refresh(
    manager: Option<&ManagerWatch>,
    watches: &mut Vec<SessionWatch>,
    changes: &Changes,
    report: &impl Fn(PcMedia),
) {
    let Some(manager) = manager else {
        report(PcMedia::default());
        return;
    };
    let result = (|| -> Result<PcMedia> {
        let (current, mut list) = sessions(&manager.manager)?;
        if let Some(current) = &current {
            if !list.contains(current) {
                list.push(current.clone());
            }
        }
        watches.retain(|watch| list.contains(&watch.session));
        for session in &list {
            if !watches.iter().any(|watch| watch.session == *session) {
                let mut watch = SessionWatch {
                    session: session.clone(),
                    playback: None,
                    properties: None,
                };
                if let Err(error) = watch.update(false, changes) {
                    tracing::warn!("media playback subscription: {error:#}");
                }
                watches.push(watch);
            }
        }
        // Read selection after subscribing, so a newly discovered player cannot change unnoticed.
        let selected = select(current.as_ref(), &list);
        for watch in watches {
            if let Err(error) = watch.update(selected == Some(&watch.session), changes) {
                tracing::warn!("media event subscription: {error:#}");
            }
        }
        selected
            .map(read_media)
            .transpose()
            .map(Option::unwrap_or_default)
    })();
    match result {
        Ok(media) => report(media),
        Err(error) => {
            tracing::warn!("read media state: {error:#}");
            report(PcMedia::default());
        }
    }
}

fn read_media(session: &Session) -> Result<PcMedia> {
    let aumid = session.SourceAppUserModelId().context("get player AUMID")?;
    let app = AppInfo::GetFromAppUserModelId(&aumid)
        .and_then(|info| info.DisplayInfo())
        .and_then(|info| info.DisplayName())
        .map(|name| name.to_string())
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| app_name(&aumid.to_string()));
    let mut media = PcMedia {
        playback: playback(session)?,
        app,
        ..PcMedia::default()
    };
    match session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
    {
        Ok(properties) => {
            media.title = properties
                .Title()
                .map(|s| s.to_string())
                .unwrap_or_default();
            media.artist = properties
                .Artist()
                .map(|s| s.to_string())
                .unwrap_or_default();
        }
        Err(error) => tracing::warn!("read media properties: {error}"),
    }
    Ok(media)
}

fn map_status(status: PlaybackStatus) -> Playback {
    match status {
        PlaybackStatus::Playing => Playback::Playing,
        PlaybackStatus::Paused => Playback::Paused,
        _ => Playback::Stopped,
    }
}

fn resolve_command(command: MediaCommand, status: Playback) -> MediaCommand {
    match command {
        MediaCommand::PlayPause if status == Playback::Playing => MediaCommand::Pause,
        MediaCommand::PlayPause => MediaCommand::Play,
        command => command,
    }
}

/// Play while playing / pause while not playing (after [`resolve_command`]).
fn is_redundant(command: MediaCommand, status: Playback) -> bool {
    match command {
        MediaCommand::Play => status == Playback::Playing,
        MediaCommand::Pause => status != Playback::Playing,
        _ => false,
    }
}

/// What the player did with a command.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Accepted,
    /// The control is disabled (e.g. no next track) or the player refused it.
    Declined,
    /// Play while already playing, pause while not playing: nothing to do. Earbuds send "play" when they connect,
    /// and a toggle key here would pause the music instead.
    Redundant,
}

/// Uses explicit Play/Pause, not toggle: Chromium often advertises only the explicit controls.
fn apply(session: &Session, command: MediaCommand) -> Result<Applied> {
    let info = session
        .GetPlaybackInfo()
        .context("get command playback info")?;
    let status = map_status(info.PlaybackStatus().context("get command status")?);
    let command = resolve_command(command, status);
    if is_redundant(command, status) {
        return Ok(Applied::Redundant);
    }
    let controls = info.Controls().context("get supported media controls")?;
    let operation = match command {
        MediaCommand::Play if controls.IsPlayEnabled()? => session.TryPlayAsync(),
        MediaCommand::Pause if controls.IsPauseEnabled()? => session.TryPauseAsync(),
        MediaCommand::Next if controls.IsNextEnabled()? => session.TrySkipNextAsync(),
        MediaCommand::Previous if controls.IsPreviousEnabled()? => session.TrySkipPreviousAsync(),
        _ => return Ok(Applied::Declined),
    };
    let accepted = operation
        .context("send GSMTC command")?
        .join()
        .context("complete GSMTC command")?;
    Ok(if accepted { Applied::Accepted } else { Applied::Declined })
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Gsmtc(Applied),
    HardwareKeyFallback,
}

/// Exactly one action per command: the media session's answer is final; the hardware media key is used only when
/// there is no media session (`Ok(None)`) or GSMTC itself failed. A declined command never falls back: the key
/// would reach the same player or, worse, toggle another one.
fn finish_command(attempt: Result<Option<Applied>>, fallback: impl FnOnce() -> Result<()>) -> Result<Route> {
    match attempt {
        Ok(Some(applied)) => Ok(Route::Gsmtc(applied)),
        Ok(None) | Err(_) => {
            fallback()?;
            Ok(Route::HardwareKeyFallback)
        }
    }
}

fn send_key(command: MediaCommand) -> Result<()> {
    let key = match command {
        MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
        MediaCommand::Previous => VK_MEDIA_PREV_TRACK,
        MediaCommand::Play | MediaCommand::Pause | MediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
    };
    let down = KEYBDINPUT {
        wVk: key,
        ..Default::default()
    };
    let up = KEYBDINPUT {
        dwFlags: KEYEVENTF_KEYUP,
        ..down
    };
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: down },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: up },
        },
    ];
    // SAFETY: two initialized keyboard INPUTs with the correct ABI size, alive for the call.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        bail!("SendInput inserted {sent}/2 media key events (desktop may be locked or UIPI may block input)");
    }
    Ok(())
}

fn app_name(aumid: &str) -> String {
    let name = aumid.rsplit(['\\', '/']).next().unwrap_or_default();
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        return name[..name.len() - 4].to_owned();
    }
    // Packaged apps: `Family_hash!AppId` → the last part of the app id.
    if let Some((_, app)) = name.rsplit_once('!') {
        return app.rsplit('.').next().unwrap_or(app).to_owned();
    }
    // A plain id such as `ru.yandex.desktop.music`: shown as is (its last segment alone means nothing).
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn playing_current_wins_over_other_players() {
        assert_eq!(
            choose_session(Some(Playback::Playing), [Playback::Playing].into_iter()),
            Some(Selection::Current)
        );
    }

    #[test]
    fn first_playing_session_wins_over_paused_current() {
        assert_eq!(
            choose_session(
                Some(Playback::Paused),
                [Playback::Stopped, Playback::Playing, Playback::Playing].into_iter()
            ),
            Some(Selection::Listed(1))
        );
    }

    #[test]
    fn current_is_used_only_when_nothing_is_playing() {
        assert_eq!(
            choose_session(
                Some(Playback::Paused),
                [Playback::Stopped, Playback::Paused].into_iter()
            ),
            Some(Selection::Current)
        );
        assert_eq!(choose_session(None, [Playback::Paused].into_iter()), None);
        assert_eq!(
            choose_session(None, [Playback::Playing].into_iter()),
            Some(Selection::Listed(0))
        );
        assert_eq!(choose_session(None, [].into_iter()), None);
    }

    #[test]
    fn toggle_uses_real_status_and_explicit_commands_stay_explicit() {
        for status in [
            Playback::None,
            Playback::Stopped,
            Playback::Paused,
            Playback::Playing,
        ] {
            let expected = if status == Playback::Playing {
                MediaCommand::Pause
            } else {
                MediaCommand::Play
            };
            assert_eq!(resolve_command(MediaCommand::PlayPause, status), expected);
            for command in [
                MediaCommand::Play,
                MediaCommand::Pause,
                MediaCommand::Next,
                MediaCommand::Previous,
            ] {
                assert_eq!(resolve_command(command, status), command);
            }
        }
    }

    #[test]
    fn maps_windows_playback_states() {
        assert_eq!(map_status(PlaybackStatus::Playing), Playback::Playing);
        assert_eq!(map_status(PlaybackStatus::Paused), Playback::Paused);
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Closed,
            PlaybackStatus::Opened,
            PlaybackStatus::Changing,
        ] {
            assert_eq!(map_status(status), Playback::Stopped);
        }
    }

    #[test]
    fn player_names_never_expose_paths() {
        assert_eq!(
            app_name(r"C:\Users\someone\Яндекс Музыка.exe"),
            "Яндекс Музыка"
        );
        assert_eq!(app_name("ru.yandex.desktop.music"), "ru.yandex.desktop.music");
        assert_eq!(
            app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "ZuneMusic"
        );
        assert_eq!(app_name("C:/players/Player.EXE"), "Player");
        assert_eq!(app_name(""), "");
    }

    #[test]
    fn a_media_session_answer_never_sends_a_hardware_key() {
        // accepted, declined (disabled control / player said no) and redundant (play while playing) are all final
        for applied in [Applied::Accepted, Applied::Declined, Applied::Redundant] {
            let keys = Cell::new(0);
            let route = finish_command(Ok(Some(applied)), || {
                keys.set(keys.get() + 1);
                Ok(())
            })
            .unwrap();
            assert!(matches!(route, Route::Gsmtc(_)));
            assert_eq!(keys.get(), 0);
        }
    }

    #[test]
    fn no_session_or_failed_gsmtc_sends_exactly_one_hardware_key() {
        for attempt in [Ok(None), Err(anyhow::anyhow!("session disappeared"))] {
            let keys = Cell::new(0);
            assert_eq!(
                finish_command(attempt, || {
                    keys.set(keys.get() + 1);
                    Ok(())
                })
                .unwrap(),
                Route::HardwareKeyFallback
            );
            assert_eq!(keys.get(), 1);
        }
        assert!(finish_command(Ok(None), || bail!("input blocked")).is_err());
    }

    #[test]
    fn play_while_playing_and_pause_while_paused_do_nothing() {
        assert!(is_redundant(MediaCommand::Play, Playback::Playing));
        assert!(!is_redundant(MediaCommand::Play, Playback::Paused));
        assert!(is_redundant(MediaCommand::Pause, Playback::Paused));
        assert!(is_redundant(MediaCommand::Pause, Playback::Stopped));
        assert!(!is_redundant(MediaCommand::Pause, Playback::Playing));
        for status in [Playback::Playing, Playback::Paused] {
            // a toggle is never redundant once resolved against the real state
            assert!(!is_redundant(resolve_command(MediaCommand::PlayPause, status), status));
            assert!(!is_redundant(MediaCommand::Next, status));
            assert!(!is_redundant(MediaCommand::Previous, status));
        }
    }
}
