package app.audiobridge.ui

import android.os.Build
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

private val FallbackDark = darkColorScheme(
    primary = Color(0xFFB9B4FF),
    onPrimary = Color(0xFF1E1470),
    primaryContainer = Color(0xFF3D2BD6),
    onPrimaryContainer = Color(0xFFE4E0FF),
    secondary = Color(0xFF6FE3EE),
    onSecondary = Color(0xFF00363B),
    secondaryContainer = Color(0xFF0D4F56),
    onSecondaryContainer = Color(0xFFB8F4FA),
    tertiary = Color(0xFFFFB4A9),
    background = Color(0xFF0F1117),
    onBackground = Color(0xFFE4E2EC),
    surface = Color(0xFF0F1117),
    onSurface = Color(0xFFE4E2EC),
    surfaceVariant = Color(0xFF2A2C38),
    onSurfaceVariant = Color(0xFFC6C5D3),
    surfaceContainerLow = Color(0xFF171922),
    surfaceContainer = Color(0xFF1B1D27),
    surfaceContainerHigh = Color(0xFF232532),
    outline = Color(0xFF8F8F9D),
    error = Color(0xFFFFB4AB),
)

/** Status colours that stay readable on every dark scheme. */
object StatusColors {
    val connected = Color(0xFF5BD68A)
    val connecting = Color(0xFFFFC857)
    val offline = Color(0xFFFF6B6B)
    val live = Color(0xFFFF5370)
}

private val BaseTypography = Typography()

private val AppTypography = Typography(
    displaySmall = BaseTypography.displaySmall.copy(fontWeight = FontWeight.SemiBold, fontSize = 34.sp, lineHeight = 42.sp),
    headlineMedium = BaseTypography.headlineMedium.copy(fontWeight = FontWeight.SemiBold),
    headlineSmall = BaseTypography.headlineSmall.copy(fontWeight = FontWeight.SemiBold),
    titleLarge = BaseTypography.titleLarge.copy(fontWeight = FontWeight.SemiBold, fontSize = 24.sp),
    titleMedium = BaseTypography.titleMedium.copy(fontWeight = FontWeight.SemiBold, fontSize = 18.sp),
    bodyLarge = BaseTypography.bodyLarge.copy(fontSize = 17.sp, lineHeight = 25.sp),
    bodyMedium = BaseTypography.bodyMedium.copy(fontSize = 15.sp, lineHeight = 21.sp),
    labelLarge = TextStyle(fontWeight = FontWeight.SemiBold, fontSize = 17.sp, letterSpacing = 0.2.sp),
)

@Composable
fun AudioBridgeTheme(content: @Composable () -> Unit) {
    val scheme: ColorScheme = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        dynamicDarkColorScheme(LocalContext.current)
    } else {
        FallbackDark
    }
    MaterialTheme(colorScheme = scheme, typography = AppTypography, content = content)
}
