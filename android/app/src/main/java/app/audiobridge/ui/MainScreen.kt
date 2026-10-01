package app.audiobridge.ui

import androidx.annotation.StringRes
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.audiobridge.BridgeStatus
import app.audiobridge.PairedPc
import app.audiobridge.PeerStatus
import app.audiobridge.R
import app.audiobridge.Reachability
import app.audiobridge.pathLabel
import kotlin.math.roundToInt

/** One missing background-operation prerequisite. */
data class SetupItem(
    val key: String,
    @param:StringRes val title: Int,
    @param:StringRes val body: Int,
    @param:StringRes val primaryLabel: Int,
    val onPrimary: () -> Unit,
    @param:StringRes val secondaryLabel: Int? = null,
    val onSecondary: (() -> Unit)? = null,
)

/** Everything the mic row needs, resolved by the activity. */
data class MicUiState(
    val userEnabled: Boolean,
    val permissionGranted: Boolean,
    val foregroundGranted: Boolean,
)

/** A paired PC joined with its live status (null until the native hub reports it). */
private data class PcRow(val pc: PairedPc, val peer: PeerStatus?) {
    val name: String get() = peer?.name?.takeIf { it.isNotBlank() } ?: pc.name.ifBlank { pc.id.take(8) }
    val reachability: Reachability get() = peer?.reachability ?: Reachability.Connecting
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(
    pcs: List<PairedPc>,
    status: BridgeStatus,
    mic: MicUiState,
    setupItems: List<SetupItem>,
    snackbarHostState: SnackbarHostState,
    onAddPc: () -> Unit,
    onRemovePc: (String) -> Unit,
    onMicToggle: (Boolean) -> Unit,
) {
    var removeId by rememberSaveable { mutableStateOf<String?>(null) }
    val rows = pcs.map { PcRow(it, status.peer(it.id)) }

    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        snackbarHost = { SnackbarHost(snackbarHostState) },
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.app_name), style = MaterialTheme.typography.titleLarge) },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            if (rows.isEmpty()) {
                Onboarding(onAddPc)
            } else {
                OverviewCard(rows, status)
                MicCard(rows, status, mic, onMicToggle)
                SectionHeader(stringResource(R.string.pcs_title), rows.size)
                rows.forEach { row -> key(row.pc.id) { PcCard(row, onRemove = { removeId = row.pc.id }) } }
                AddPcButton(onAddPc)
                AnimatedVisibility(visible = setupItems.isNotEmpty()) {
                    SetupCard(setupItems)
                }
            }
            Spacer(Modifier.height(16.dp))
        }
    }

    val target = rows.firstOrNull { it.pc.id == removeId }
    if (target != null) {
        AlertDialog(
            onDismissRequest = { removeId = null },
            icon = { Icon(painterResource(R.drawable.ic_delete), contentDescription = null) },
            title = { Text(stringResource(R.string.remove_title, target.name)) },
            text = { Text(stringResource(R.string.remove_body)) },
            confirmButton = {
                TextButton(onClick = {
                    removeId = null
                    onRemovePc(target.pc.id)
                }) { Text(stringResource(R.string.remove)) }
            },
            dismissButton = {
                TextButton(onClick = { removeId = null }) { Text(stringResource(R.string.cancel)) }
            },
        )
    }
}

@Composable
private fun Onboarding(onScan: () -> Unit) {
    Column(
        modifier = Modifier.fillMaxWidth().padding(top = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Image(
            painter = painterResource(R.drawable.hero),
            contentDescription = null,
            modifier = Modifier.fillMaxWidth().heightIn(max = 220.dp),
        )
        Spacer(Modifier.height(28.dp))
        Text(
            stringResource(R.string.hero_title),
            style = MaterialTheme.typography.displaySmall,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(14.dp))
        Text(
            stringResource(R.string.hero_body),
            style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(36.dp))
        Button(
            onClick = onScan,
            modifier = Modifier.fillMaxWidth().height(64.dp),
            shape = RoundedCornerShape(20.dp),
            contentPadding = PaddingValues(horizontal = 24.dp),
        ) {
            Icon(painterResource(R.drawable.ic_qr), contentDescription = null, modifier = Modifier.size(26.dp))
            Spacer(Modifier.width(12.dp))
            Text(stringResource(R.string.scan_qr), style = MaterialTheme.typography.labelLarge)
        }
        Spacer(Modifier.height(12.dp))
        Text(
            stringResource(R.string.scan_hint),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}

private fun reachabilityColor(reach: Reachability): Color = when (reach) {
    Reachability.Connected -> StatusColors.connected
    Reachability.Connecting -> StatusColors.connecting
    Reachability.Offline -> StatusColors.offline
}

@StringRes
private fun reachabilityText(reach: Reachability): Int = when (reach) {
    Reachability.Connected -> R.string.state_connected
    Reachability.Connecting -> R.string.state_connecting
    Reachability.Offline -> R.string.state_offline
}

/** Big summary: how many PCs are connected and whether audio is playing. */
@Composable
private fun OverviewCard(rows: List<PcRow>, status: BridgeStatus) {
    val connected = rows.count { it.reachability == Reachability.Connected }
    val connecting = rows.any { it.reachability == Reachability.Connecting }
    val overall = when {
        connected > 0 -> Reachability.Connected
        connecting -> Reachability.Connecting
        else -> Reachability.Offline
    }
    val title = when {
        connected == rows.size -> if (rows.size == 1) stringResource(R.string.state_connected) else stringResource(R.string.overview_all_connected)
        connected > 0 -> stringResource(R.string.overview_connected, connected, rows.size)
        connecting -> stringResource(R.string.overview_connecting)
        else -> stringResource(R.string.overview_offline)
    }
    val playing = connected > 0 && status.pcAudioActive
    val subtitle = when {
        playing -> R.string.overview_audio_playing
        overall == Reachability.Connected -> R.string.overview_audio_idle
        else -> R.string.overview_offline_hint
    }
    val accent by animateColorAsState(reachabilityColor(overall), label = "overviewColor")
    val scheme = MaterialTheme.colorScheme

    Card(
        shape = RoundedCornerShape(32.dp),
        colors = CardDefaults.cardColors(containerColor = Color.Transparent),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Box(
            Modifier
                .fillMaxWidth()
                .background(Brush.linearGradient(listOf(scheme.primaryContainer, scheme.surfaceContainerHigh)))
                .padding(24.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(64.dp).clip(CircleShape).background(scheme.surface.copy(alpha = 0.35f)),
                    contentAlignment = Alignment.Center,
                ) {
                    if (playing) {
                        Equalizer(scheme.onPrimaryContainer)
                    } else {
                        Icon(
                            painterResource(R.drawable.ic_headphones),
                            contentDescription = null,
                            tint = scheme.onPrimaryContainer,
                            modifier = Modifier.size(30.dp),
                        )
                    }
                }
                Spacer(Modifier.width(18.dp))
                Column(Modifier.weight(1f)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        StatusDot(accent, pulsing = overall == Reachability.Connecting, size = 10)
                        Spacer(Modifier.width(8.dp))
                        Text(
                            title,
                            style = MaterialTheme.typography.headlineSmall,
                            color = scheme.onPrimaryContainer,
                        )
                    }
                    Spacer(Modifier.height(4.dp))
                    Text(
                        stringResource(subtitle),
                        style = MaterialTheme.typography.bodyMedium,
                        color = scheme.onPrimaryContainer.copy(alpha = 0.8f),
                    )
                }
            }
        }
    }
}

/** Three bouncing bars shown while PC audio is playing. */
@Composable
private fun Equalizer(color: Color) {
    val transition = rememberInfiniteTransition(label = "eq")
    val durations = listOf(520, 680, 440)
    Row(
        modifier = Modifier.height(26.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.Bottom,
    ) {
        durations.forEachIndexed { i, duration ->
            val fraction by transition.animateFloat(
                initialValue = 0.25f,
                targetValue = 1f,
                animationSpec = infiniteRepeatable(tween(duration, easing = FastOutSlowInEasing), RepeatMode.Reverse),
                label = "bar$i",
            )
            Box(
                Modifier
                    .width(5.dp)
                    .fillMaxHeight(fraction)
                    .clip(RoundedCornerShape(3.dp))
                    .background(color),
            )
        }
    }
}

@Composable
private fun SectionHeader(title: String, count: Int) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 8.dp, start = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(title, style = MaterialTheme.typography.titleLarge)
        Spacer(Modifier.width(10.dp))
        Surface(shape = CircleShape, color = MaterialTheme.colorScheme.secondaryContainer) {
            Text(
                count.toString(),
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
                modifier = Modifier.padding(horizontal = 10.dp, vertical = 2.dp),
            )
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PcCard(row: PcRow, onRemove: () -> Unit) {
    val reach = row.reachability
    val color by animateColorAsState(reachabilityColor(reach), label = "pcColor")
    val peer = row.peer
    val connected = reach == Reachability.Connected && peer != null
    val stateText = buildString {
        append(stringResource(reachabilityText(reach)))
        val path = peer?.path
        if (connected && path != null) append(" · ").append(stringResource(pathLabel(path)))
    }

    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        modifier = Modifier.fillMaxWidth().animateContentSize(),
    ) {
        Column(Modifier.padding(start = 20.dp, end = 8.dp, top = 18.dp, bottom = 18.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(52.dp).clip(RoundedCornerShape(16.dp)).background(color.copy(alpha = 0.16f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(painterResource(R.drawable.ic_computer), contentDescription = null, tint = color)
                }
                Spacer(Modifier.width(16.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        row.name,
                        style = MaterialTheme.typography.titleLarge,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Spacer(Modifier.height(2.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        StatusDot(color, pulsing = reach == Reachability.Connecting, size = 8)
                        Spacer(Modifier.width(6.dp))
                        Text(
                            stateText,
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                IconButton(onClick = onRemove) {
                    Icon(
                        painterResource(R.drawable.ic_delete),
                        contentDescription = stringResource(R.string.remove),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            if (connected) {
                Spacer(Modifier.height(14.dp))
                FlowRow(
                    modifier = Modifier.padding(end = 12.dp),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    peer.rttMs?.let { Chip(stringResource(R.string.stat_ping, it.roundToInt())) }
                    when {
                        !peer.pcAudioEnabled -> Chip(stringResource(R.string.stat_audio_off))
                        peer.pcAudio.active -> {
                            Chip(stringResource(R.string.stat_audio), emphasized = true)
                            Chip(stringResource(R.string.stat_buffer, peer.pcAudio.bufferMs.roundToInt()))
                        }
                    }
                    if (peer.micDemanded) MicBadge()
                }
            } else {
                val error = peer?.error
                if (reach == Reachability.Offline && !error.isNullOrBlank()) {
                    Spacer(Modifier.height(10.dp))
                    Text(
                        error,
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.padding(end = 12.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun Chip(text: String, emphasized: Boolean = false) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        shape = RoundedCornerShape(10.dp),
        color = if (emphasized) scheme.secondaryContainer else scheme.surfaceContainerHighest,
    ) {
        Text(
            text,
            style = MaterialTheme.typography.bodyMedium,
            color = if (emphasized) scheme.onSecondaryContainer else scheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 5.dp),
        )
    }
}

@Composable
private fun MicBadge() {
    Surface(shape = RoundedCornerShape(10.dp), color = StatusColors.live.copy(alpha = 0.18f)) {
        Row(
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 5.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                painterResource(R.drawable.ic_mic),
                contentDescription = null,
                tint = StatusColors.live,
                modifier = Modifier.size(16.dp),
            )
            Spacer(Modifier.width(4.dp))
            Text(stringResource(R.string.mic_badge), style = MaterialTheme.typography.bodyMedium, color = StatusColors.live)
        }
    }
}

@Composable
private fun AddPcButton(onAdd: () -> Unit) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.fillMaxWidth()) {
        FilledTonalButton(
            onClick = onAdd,
            modifier = Modifier.fillMaxWidth().height(60.dp),
            shape = RoundedCornerShape(20.dp),
        ) {
            Icon(painterResource(R.drawable.ic_add), contentDescription = null, modifier = Modifier.size(24.dp))
            Spacer(Modifier.width(10.dp))
            Text(stringResource(R.string.add_pc), style = MaterialTheme.typography.labelLarge)
        }
        Spacer(Modifier.height(8.dp))
        Text(
            stringResource(R.string.add_pc_hint),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}

@Composable
private fun StatusDot(color: Color, pulsing: Boolean, size: Int = 12) {
    val alpha = if (pulsing) {
        val transition = rememberInfiniteTransition(label = "pulse")
        transition.animateFloat(
            initialValue = 1f,
            targetValue = 0.3f,
            animationSpec = infiniteRepeatable(tween(800, easing = FastOutSlowInEasing), RepeatMode.Reverse),
            label = "pulseAlpha",
        ).value
    } else {
        1f
    }
    Box(
        Modifier
            .size(size.dp)
            .alpha(alpha)
            .clip(CircleShape)
            .background(color),
    )
}

@Composable
private fun MicCard(rows: List<PcRow>, status: BridgeStatus, mic: MicUiState, onToggle: (Boolean) -> Unit) {
    val checked = mic.userEnabled && mic.permissionGranted
    val live = checked && mic.foregroundGranted && status.micCapturing
    val users = rows.filter { it.peer?.micInUse == true }.joinToString(", ") { it.name }
    val subtitle = when {
        !mic.userEnabled -> stringResource(R.string.mic_off)
        !mic.permissionGranted -> stringResource(R.string.mic_need_permission)
        live && users.isNotEmpty() -> stringResource(R.string.mic_in_use_on, users)
        live -> stringResource(R.string.mic_in_use)
        else -> stringResource(R.string.mic_waiting)
    }
    val iconBg = if (live) StatusColors.live else MaterialTheme.colorScheme.secondaryContainer
    val iconFg = if (live) Color.White else MaterialTheme.colorScheme.onSecondaryContainer

    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .toggleable(value = checked, role = Role.Switch, onValueChange = onToggle)
                .padding(horizontal = 20.dp, vertical = 18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(48.dp).clip(CircleShape).background(iconBg),
                contentAlignment = Alignment.Center,
            ) {
                Icon(painterResource(R.drawable.ic_mic), contentDescription = null, tint = iconFg)
            }
            Spacer(Modifier.width(16.dp))
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.mic_title), style = MaterialTheme.typography.titleMedium)
                Spacer(Modifier.height(2.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (live) {
                        StatusDot(StatusColors.live, pulsing = true, size = 8)
                        Spacer(Modifier.width(6.dp))
                    }
                    Text(
                        subtitle,
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (live) StatusColors.live else MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            Spacer(Modifier.width(12.dp))
            Switch(checked = checked, onCheckedChange = null)
        }
    }
}

@Composable
private fun SetupCard(items: List<SetupItem>) {
    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(18.dp)) {
            Text(stringResource(R.string.setup_title), style = MaterialTheme.typography.titleMedium)
            items.forEach { item -> key(item.key) { SetupRow(item) } }
        }
    }
}

@Composable
private fun SetupRow(item: SetupItem) {
    Column {
        Text(stringResource(item.title), style = MaterialTheme.typography.bodyLarge)
        Text(
            stringResource(item.body),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilledTonalButton(onClick = item.onPrimary) { Text(stringResource(item.primaryLabel)) }
            val secondary = item.secondaryLabel
            val onSecondary = item.onSecondary
            if (secondary != null && onSecondary != null) {
                TextButton(onClick = onSecondary) {
                    Icon(painterResource(R.drawable.ic_check), contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text(stringResource(secondary))
                }
            }
        }
    }
}
