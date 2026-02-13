use gpui::{
  App, Hsla, IntoElement, RenderOnce, SharedString, Styled, Window, prelude::*, rems, svg, Rems,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconName {
  ArrowLeft,
}

impl IconName {
  pub fn path(self) -> SharedString {
    let name = match self {
      IconName::ArrowLeft => "arrow-left",
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

  pub fn size(mut self, size: Rems) -> Self {
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
    svg()
      .path(self.name.path())
      .size(self.size)
      .flex_none()
      .when_some(self.color, |this, color| this.text_color(color))
  }
}
