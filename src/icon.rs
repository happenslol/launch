use std::time::Duration;

use gpui::{
  Animation, AnimationExt, App, Hsla, IntoElement, Rems, RenderOnce, SharedString, Styled,
  Transformation, Window, percentage, prelude::*, rems, rgb, svg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconName {
  AppWindow,
  ArrowLeft,
  Bluetooth,
  Headphones,
  Loader,
  Lock,
  Microphone,
  Network,
  Volume,
  Wifi,
}

impl IconName {
  pub fn path(self) -> SharedString {
    let name = match self {
      IconName::AppWindow => "app-window",
      IconName::ArrowLeft => "arrow-left",
      IconName::Bluetooth => "bluetooth",
      IconName::Headphones => "headphones",
      IconName::Loader => "loader-2",
      IconName::Lock => "lock",
      IconName::Microphone => "microphone",
      IconName::Network => "network",
      IconName::Volume => "volume",
      IconName::Wifi => "wifi",
    };
    SharedString::from(format!("assets/icons/{name}.svg"))
  }
}

#[derive(IntoElement)]
pub struct Icon {
  name: IconName,
  size: Rems,
  color: Option<Hsla>,
}

impl Icon {
  pub fn new(name: IconName) -> Self {
    Self {
      name,
      size: rems(1.0),
      color: None,
    }
  }

  pub fn custom_size(mut self, size: Rems) -> Self {
    self.size = size;
    self
  }

  pub fn color(mut self, color: Hsla) -> Self {
    self.color = Some(color);
    self
  }
}

impl RenderOnce for Icon {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    let color = self.color.unwrap_or(rgb(0x555555).into());

    svg()
      .path(self.name.path())
      .size(self.size)
      .flex_none()
      .text_color(color)
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
