//! Media transport over MPRIS (D-Bus session bus): headphone buttons on the phone control the
//! PC's player (browsers, Spotify, mpv, Yandex Music clients…), and the phone sees what it plays.
//! Event-driven: `PropertiesChanged` and `NameOwnerChanged` signals trigger a re-read.

use std::collections::HashMap;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use audiobridge_core::session::{MediaCommand, PcMedia, Playback};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Type as MessageType;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";

enum Message {
    Command(MediaCommand),
    Shutdown,
}

#[derive(Clone)]
pub struct Sender(mpsc::UnboundedSender<Message>);

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
        let (tx, rx) = mpsc::unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("media-transport".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt.block_on(run(rx, report)),
                    Err(e) => tracing::warn!("media transport runtime: {e}"),
                }
            })
            .context("spawn media transport thread")?;
        Ok(Self { sender: Sender(tx), thread })
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

async fn run(mut rx: mpsc::UnboundedReceiver<Message>, report: impl Fn(PcMedia)) {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("D-Bus session bus unavailable; headphone buttons cannot control players: {e}");
            report(PcMedia::default());
            while let Some(Message::Command(command)) = rx.recv().await {
                tracing::info!(?command, "media command ignored: no D-Bus session bus");
            }
            return;
        }
    };
    let (mut props, mut owners) = match signals(&conn).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("MPRIS signal subscription: {e:#}");
            (None, None)
        }
    };
    let mut players = Players::default();
    let mut last = None;
    refresh(&conn, &mut players, &report, &mut last).await;
    loop {
        tokio::select! {
            message = rx.recv() => match message {
                None | Some(Message::Shutdown) => break,
                Some(Message::Command(command)) => {
                    apply(&conn, &mut players, command).await;
                    refresh(&conn, &mut players, &report, &mut last).await;
                }
            },
            Some(_) = next(&mut props) => refresh(&conn, &mut players, &report, &mut last).await,
            Some(_) = next(&mut owners) => refresh(&conn, &mut players, &report, &mut last).await,
        }
    }
}

/// The next message of an optional stream; pending forever without one.
async fn next(stream: &mut Option<MessageStream>) -> Option<()> {
    match stream {
        Some(s) => s.next().await.map(|_| ()),
        None => std::future::pending().await,
    }
}

async fn signals(conn: &Connection) -> Result<(Option<MessageStream>, Option<MessageStream>)> {
    let props = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path(MPRIS_PATH)?
        .build();
    let owners = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns("org.mpris.MediaPlayer2")?
        .build();
    Ok((
        Some(MessageStream::for_match_rule(props, conn, Some(64)).await?),
        Some(MessageStream::for_match_rule(owners, conn, Some(64)).await?),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Player {
    bus_name: String,
    playback: Playback,
}

#[derive(Default)]
struct Players {
    list: Vec<Player>,
    /// The player that played or was controlled most recently.
    last_active: Option<String>,
}

impl Players {
    fn selected(&self) -> Option<&Player> {
        select(&self.list, self.last_active.as_deref()).map(|i| &self.list[i])
    }
}

/// Exactly one target, like the Windows media session: a playing player (the most recently
/// active one if several play), else the most recently active one, else the first one.
fn select(list: &[Player], last_active: Option<&str>) -> Option<usize> {
    let last = last_active.and_then(|name| list.iter().position(|p| p.bus_name == name));
    let playing = |i: &usize| list[*i].playback == Playback::Playing;
    last.filter(playing)
        .or_else(|| (0..list.len()).find(playing))
        .or(last)
        .or((!list.is_empty()).then_some(0))
}

async fn proxy<'a>(conn: &Connection, bus_name: &'a str, iface: &'a str) -> zbus::Result<Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination(bus_name)?
        .path(MPRIS_PATH)?
        .interface(iface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn bus_names(conn: &Connection) -> zbus::Result<Vec<String>> {
    let reply = conn
        .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "ListNames", &())
        .await?;
    let names: Vec<String> = reply.body().deserialize()?;
    Ok(names.into_iter().filter(|n| n.starts_with(MPRIS_PREFIX)).collect())
}

fn map_status(status: &str) -> Playback {
    match status {
        "Playing" => Playback::Playing,
        "Paused" => Playback::Paused,
        _ => Playback::Stopped,
    }
}

async fn playback(conn: &Connection, bus_name: &str) -> zbus::Result<Playback> {
    let p = proxy(conn, bus_name, PLAYER_IFACE).await?;
    Ok(map_status(&p.get_property::<String>("PlaybackStatus").await?))
}

async fn refresh(conn: &Connection, players: &mut Players, report: &impl Fn(PcMedia), last: &mut Option<PcMedia>) {
    let names = match bus_names(conn).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("listing MPRIS players: {e}");
            Vec::new()
        }
    };
    let mut list = Vec::with_capacity(names.len());
    for bus_name in names {
        match playback(conn, &bus_name).await {
            Ok(playback) => list.push(Player { bus_name, playback }),
            Err(e) => tracing::debug!("MPRIS player {bus_name}: {e}"),
        }
    }
    players.list = list;
    if let Some(playing) = players.list.iter().find(|p| p.playback == Playback::Playing) {
        let keep = players.selected().is_some_and(|s| s.playback == Playback::Playing);
        if !keep {
            players.last_active = Some(playing.bus_name.clone());
        }
    }
    let media = match players.selected() {
        Some(p) => read_media(conn, p).await.unwrap_or_else(|e| {
            tracing::debug!("reading MPRIS metadata of {}: {e}", p.bus_name);
            PcMedia { playback: p.playback, app: app_name(&p.bus_name), ..PcMedia::default() }
        }),
        None => PcMedia::default(),
    };
    if last.as_ref() != Some(&media) {
        tracing::debug!(?media, "media state");
        report(media.clone());
        *last = Some(media);
    }
}

/// Fallback display name from the bus name: `org.mpris.MediaPlayer2.spotify` → "spotify",
/// without the `.instance123` suffix browsers add.
fn app_name(bus_name: &str) -> String {
    let rest = bus_name.strip_prefix(MPRIS_PREFIX).unwrap_or(bus_name);
    rest.split('.').next().unwrap_or(rest).to_owned()
}

async fn read_media(conn: &Connection, player: &Player) -> zbus::Result<PcMedia> {
    let root = proxy(conn, &player.bus_name, ROOT_IFACE).await?;
    let app = root
        .get_property::<String>("Identity")
        .await
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| app_name(&player.bus_name));
    let p = proxy(conn, &player.bus_name, PLAYER_IFACE).await?;
    let metadata: HashMap<String, OwnedValue> = p.get_property("Metadata").await.unwrap_or_default();
    let title = metadata.get("xesam:title").map(|v| strings(v).join(" ")).unwrap_or_default();
    let artist = metadata.get("xesam:artist").map(|v| strings(v).join(", ")).unwrap_or_default();
    Ok(PcMedia { playback: player.playback, app, title, artist })
}

/// A string or an array of strings (`xesam:artist` is `as`).
fn strings(value: &Value<'_>) -> Vec<String> {
    match value {
        Value::Str(s) => vec![s.to_string()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        Value::Value(v) => strings(v),
        _ => Vec::new(),
    }
}

fn resolve_command(command: MediaCommand, status: Playback) -> MediaCommand {
    match command {
        MediaCommand::PlayPause if status == Playback::Playing => MediaCommand::Pause,
        MediaCommand::PlayPause => MediaCommand::Play,
        command => command,
    }
}

/// Play while playing / pause while not playing (after [`resolve_command`]): earbuds send
/// "play" when they connect, and acting on it would toggle the music.
fn is_redundant(command: MediaCommand, status: Playback) -> bool {
    match command {
        MediaCommand::Play => status == Playback::Playing,
        MediaCommand::Pause => status != Playback::Playing,
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Accepted,
    /// The player does not allow it (e.g. no next track).
    Declined,
    Redundant,
}

async fn apply(conn: &Connection, players: &mut Players, command: MediaCommand) {
    let Some(player) = players.selected().cloned() else {
        tracing::info!(?command, "media command not applied: no MPRIS player");
        return;
    };
    // Re-read: the cached list may be a signal behind.
    let status = playback(conn, &player.bus_name).await.unwrap_or(player.playback);
    let result = send(conn, &player.bus_name, resolve_command(command, status), status).await;
    match result {
        Ok(applied) => {
            if applied == Applied::Accepted {
                players.last_active = Some(player.bus_name.clone());
            }
            tracing::info!(?command, player = %player.bus_name, ?applied, "media command");
        }
        Err(e) => tracing::info!(?command, player = %player.bus_name, "media command failed: {e}"),
    }
}

async fn send(conn: &Connection, bus_name: &str, command: MediaCommand, status: Playback) -> zbus::Result<Applied> {
    if is_redundant(command, status) {
        return Ok(Applied::Redundant);
    }
    let (can, method) = match command {
        MediaCommand::Play | MediaCommand::PlayPause => ("CanPlay", "Play"),
        MediaCommand::Pause => ("CanPause", "Pause"),
        MediaCommand::Next => ("CanGoNext", "Next"),
        MediaCommand::Previous => ("CanGoPrevious", "Previous"),
    };
    let p = proxy(conn, bus_name, PLAYER_IFACE).await?;
    let allowed = p.get_property::<bool>("CanControl").await.unwrap_or(true) && p.get_property::<bool>(can).await.unwrap_or(true);
    if !allowed {
        return Ok(Applied::Declined);
    }
    p.call_method(method, &()).await?;
    Ok(Applied::Accepted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(name: &str, playback: Playback) -> Player {
        Player { bus_name: name.into(), playback }
    }

    #[test]
    fn exactly_one_target_prefers_playing_then_recent() {
        let list = [player("a", Playback::Paused), player("b", Playback::Playing), player("c", Playback::Playing)];
        assert_eq!(select(&list, None), Some(1));
        // the most recently active player wins among playing ones
        assert_eq!(select(&list, Some("c")), Some(2));
        let paused = [player("a", Playback::Paused), player("b", Playback::Stopped)];
        assert_eq!(select(&paused, Some("b")), Some(1));
        assert_eq!(select(&paused, Some("gone")), Some(0));
        assert_eq!(select(&[], Some("a")), None);
    }

    #[test]
    fn toggle_resolves_from_real_state_and_skips_redundant() {
        assert_eq!(resolve_command(MediaCommand::PlayPause, Playback::Playing), MediaCommand::Pause);
        assert_eq!(resolve_command(MediaCommand::PlayPause, Playback::Paused), MediaCommand::Play);
        assert!(is_redundant(MediaCommand::Play, Playback::Playing));
        assert!(is_redundant(MediaCommand::Pause, Playback::Stopped));
        assert!(!is_redundant(MediaCommand::Next, Playback::Playing));
    }

    #[test]
    fn browser_instances_show_a_readable_name() {
        assert_eq!(app_name("org.mpris.MediaPlayer2.chromium.instance1234"), "chromium");
        assert_eq!(app_name("org.mpris.MediaPlayer2.spotify"), "spotify");
    }
}
