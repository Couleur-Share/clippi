//! Native system accent color access; called on the GPUI main thread.
#[cfg(target_os = "windows")]
use gpui::rgb;
use gpui::Rgba;

#[cfg(target_os = "windows")]
pub fn read_system_accent() -> Option<Rgba> {
    use windows::UI::ViewManagement::{UIColorType, UISettings};
    let color = UISettings::new()
        .and_then(|settings| settings.GetColorValue(UIColorType::Accent))
        .ok()?;
    Some(rgb((u32::from(color.R) << 16)
        | (u32::from(color.G) << 8)
        | u32::from(color.B)))
}

#[cfg(target_os = "macos")]
pub fn read_system_accent() -> Option<Rgba> {
    use objc2_app_kit::{NSColor, NSColorSpace};
    let color = NSColor::controlAccentColor();
    let color = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(Rgba {
        r: color.redComponent() as f32,
        g: color.greenComponent() as f32,
        b: color.blueComponent() as f32,
        a: 1.0,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn read_system_accent() -> Option<Rgba> {
    None
}
