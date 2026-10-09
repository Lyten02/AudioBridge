package app.audiobridge.ui

import androidx.annotation.StringRes
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import app.audiobridge.BridgeStatus
import app.audiobridge.HeadsetSettings
import app.audiobridge.KeyAction
import app.audiobridge.MediaKeyLog
import app.audiobridge.NativeBridge
import app.audiobridge.PairedPc
import app.audiobridge.PcPlayback
import app.audiobridge.PeerState
import app.audiobridge.R
import app.audiobridge.TapAction

/** Headphone buttons → PC media control: on/off, which PC, and the single-earbud mapping. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun HeadsetCard(
    settings: HeadsetSettings,
    pcs: List<PairedPc>,
    status: BridgeStatus,
    targetId: String?,
    lastKey: MediaKeyLog?,
    onChange: (HeadsetSettings) -> Unit,
) {
    val target = targetId?.let { status.peer(it) }
    val subtitle = when {
        !settings.enabled -> stringResource(R.string.headset_off)
        target == null && status.connectedPeers.size > 1 -> stringResource(R.string.headset_no_target_many)
        target == null -> stringResource(R.string.headset_no_target)
        target.media.app.isNotBlank() -> stringResource(
            R.string.headset_target_media,
            target.name,
            target.media.app + when (target.media.playback) {
                PcPlayback.Playing -> " · " + stringResource(R.string.headset_playing)
                PcPlayback.Paused -> " · " + stringResource(R.string.headset_paused)
                else -> ""
            },
        )
        else -> stringResource(R.string.headset_target, target.name)
    }
    val active = settings.enabled && target != null
    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .toggleable(value = settings.enabled, role = Role.Switch, onValueChange = { onChange(settings.copy(enabled = it)) })
                .padding(horizontal = 20.dp, vertical = 18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(48.dp).clip(CircleShape).background(
                    if (active) MaterialTheme.colorScheme.primaryContainer else MaterialTheme.colorScheme.secondaryContainer,
                ),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painterResource(R.drawable.ic_headphones),
                    contentDescription = null,
                    tint = if (active) MaterialTheme.colorScheme.onPrimaryContainer else MaterialTheme.colorScheme.onSecondaryContainer,
                )
            }
            Spacer(Modifier.width(16.dp))
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.headset_title), style = MaterialTheme.typography.titleMedium)
                Spacer(Modifier.height(2.dp))
                Text(subtitle, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            Spacer(Modifier.width(12.dp))
            Switch(checked = settings.enabled, onCheckedChange = null)
        }
        if (!settings.enabled) return@Card
        Column(Modifier.padding(start = 20.dp, end = 20.dp, bottom = 18.dp)) {
            if (pcs.size > 1) {
                Text(stringResource(R.string.headset_pc_choice), style = MaterialTheme.typography.labelLarge)
                FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    val chosen = settings.targetId?.takeIf { id -> pcs.any { it.id == id } }
                    FilterChip(
                        selected = chosen == null,
                        onClick = { onChange(settings.copy(targetId = null)) },
                        label = { Text(stringResource(R.string.headset_pc_auto)) },
                    )
                    for (pc in pcs) {
                        val online = status.peer(pc.id)?.state == PeerState.Connected
                        FilterChip(
                            selected = chosen == pc.id,
                            onClick = { onChange(settings.copy(targetId = pc.id)) },
                            label = { Text(pc.name) },
                            enabled = online || chosen == pc.id,
                        )
                    }
                }
                if (settings.targetId == null) Note(stringResource(R.string.headset_pc_auto_hint))
                Spacer(Modifier.height(8.dp))
            }
            if (!settings.singleEarbud) Note(stringResource(R.string.headset_default_layout))
            HorizontalDivider(Modifier.padding(vertical = 8.dp))
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .toggleable(
                        value = settings.singleEarbud,
                        role = Role.Switch,
                        onValueChange = { onChange(settings.copy(singleEarbud = it)) },
                    )
                    .padding(vertical = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(stringResource(R.string.headset_single), style = MaterialTheme.typography.bodyLarge)
                    Text(
                        stringResource(R.string.headset_single_desc),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Spacer(Modifier.width(12.dp))
                Switch(checked = settings.singleEarbud, onCheckedChange = null)
            }
            if (settings.singleEarbud) {
                TapRow(stringResource(R.string.headset_tap_1)) {
                    Text(
                        stringResource(R.string.headset_tap_1_fixed),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                TapRow(stringResource(R.string.headset_tap_2)) {
                    TapPicker(settings.doubleTap) { onChange(settings.copy(doubleTap = it)) }
                }
                TapRow(stringResource(R.string.headset_tap_3)) {
                    TapPicker(settings.tripleTap) { onChange(settings.copy(tripleTap = it)) }
                }
                Note(stringResource(R.string.headset_limits))
            }
            Spacer(Modifier.height(8.dp))
            Text(lastKeyText(lastKey), style = MaterialTheme.typography.bodyMedium)
            Note(stringResource(R.string.headset_android_note))
        }
    }
}

@Composable
private fun Note(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(top = 4.dp),
    )
}

@Composable
private fun TapRow(title: String, value: @Composable () -> Unit) {
    Row(Modifier.fillMaxWidth().padding(vertical = 2.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(title, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        value()
    }
}

@Composable
private fun TapPicker(value: TapAction, onPick: (TapAction) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        TextButton(onClick = { open = true }) { Text(stringResource(tapLabel(value))) }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for (a in TapAction.entries) {
                DropdownMenuItem(
                    text = { Text(stringResource(tapLabel(a))) },
                    onClick = {
                        open = false
                        onPick(a)
                    },
                )
            }
        }
    }
}

@StringRes
fun tapLabel(a: TapAction): Int = when (a) {
    TapAction.PlayPause -> R.string.tap_play_pause
    TapAction.Next -> R.string.tap_next
    TapAction.Previous -> R.string.tap_previous
    TapAction.VolumeUp -> R.string.tap_volume_up
    TapAction.VolumeDown -> R.string.tap_volume_down
    TapAction.Nothing -> R.string.tap_nothing
}

@StringRes
private fun actionLabel(a: KeyAction): Int = when (a) {
    is KeyAction.Pc -> when (a.command) {
        NativeBridge.MEDIA_PLAY -> R.string.cmd_play
        NativeBridge.MEDIA_PAUSE -> R.string.cmd_pause
        NativeBridge.MEDIA_NEXT -> R.string.tap_next
        NativeBridge.MEDIA_PREVIOUS -> R.string.tap_previous
        else -> R.string.tap_play_pause
    }
    is KeyAction.PhoneVolume -> if (a.up) R.string.tap_volume_up else R.string.tap_volume_down
    KeyAction.Ignore -> R.string.tap_nothing
}

@Composable
private fun lastKeyText(log: MediaKeyLog?): String {
    if (log == null) return stringResource(R.string.headset_last_none)
    val what = stringResource(actionLabel(log.action))
    return if (log.action is KeyAction.Pc && log.pcName != null) {
        stringResource(R.string.headset_last_to, what, log.pcName)
    } else {
        stringResource(R.string.headset_last, what)
    }
}
