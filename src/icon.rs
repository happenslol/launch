use std::time::Duration;

use gpui::{
  Animation, AnimationExt, App, Hsla, IntoElement, RenderOnce, SharedString, StyleRefinement,
  Styled, Transformation, Window, percentage, prelude::*, rems, rgb, svg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconName {
  AppWindow,
  ArrowLeft,
  Asterisk,
  Bluetooth,
  Broadcast,
  BroadcastOff,
  Clipboard,
  CircleCheckFilled,
  DeviceDesktop,
  DeviceGamepad,
  DeviceSpeaker,
  Headphones,
  Headset,
  Keyboard,
  Loader,
  Lock,
  Microphone,
  Mouse,
  Network,
  Phone,
  Printer,
  StarFilled,
  Volume,
  Wifi,
}

impl IconName {
  pub fn path(self) -> SharedString {
    let name = match self {
      IconName::AppWindow => "app-window",
      IconName::ArrowLeft => "arrow-left",
      IconName::Asterisk => "asterisk",
      IconName::Bluetooth => "bluetooth",
      IconName::Broadcast => "broadcast",
      IconName::BroadcastOff => "broadcast-off",
      IconName::Clipboard => "clipboard",
      IconName::CircleCheckFilled => "circle-check-filled",
      IconName::DeviceDesktop => "device-desktop",
      IconName::DeviceGamepad => "device-gamepad",
      IconName::DeviceSpeaker => "device-speaker",
      IconName::Headphones => "headphones",
      IconName::Headset => "headset",
      IconName::Keyboard => "keyboard",
      IconName::Loader => "loader-2",
      IconName::Lock => "lock",
      IconName::Microphone => "microphone",
      IconName::Mouse => "mouse",
      IconName::Network => "network",
      IconName::Phone => "phone",
      IconName::Printer => "printer",
      IconName::StarFilled => "star-filled",
      IconName::Volume => "volume",
      IconName::Wifi => "wifi",
    };
    SharedString::from(format!("assets/icons/{name}.svg"))
  }
}

#[derive(IntoElement)]
pub struct Icon {
  name: IconName,
  style: StyleRefinement,
}

impl Icon {
  pub fn new(name: IconName) -> Self {
    Self {
      name,
      style: StyleRefinement::default(),
    }
  }
}

impl Styled for Icon {
  fn style(&mut self) -> &mut StyleRefinement {
    &mut self.style
  }
}

impl RenderOnce for Icon {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    let color = self.style.text.color.unwrap_or(rgb(0x555555).into());
    svg().path(self.name.path()).flex_none().text_color(color)
  }
}

#[derive(IntoElement)]
pub struct Spinner {
  color: Option<Hsla>,
}

impl Spinner {
  pub fn new() -> Self {
    Self { color: None }
  }

  pub fn color(mut self, color: Hsla) -> Self {
    self.color = Some(color);
    self
  }
}

impl RenderOnce for Spinner {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    svg()
      .path(IconName::Loader.path())
      .size(rems(1.0))
      .flex_none()
      .when_some(self.color, |this, color| this.text_color(color))
      .with_animation(
        "spinner",
        Animation::new(Duration::from_millis(800)).repeat(),
        |this, delta| this.with_transformation(Transformation::rotate(percentage(delta))),
      )
  }
}
