use std::any::TypeId;
use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, rgba, AnyElement, App, Bounds,
    Context, CursorStyle, DispatchPhase, Element, Entity, FocusHandle, Focusable, FontStyle,
    FontWeight, GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior, IntoElement, KeyContext, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString,
    StrikethroughStyle, StyledText, TextLayout, Window,
};

use crate::wayland;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const MARKDOWN_CONTEXT: &str = "markdown";

actions!(markdown, [Copy]);

#[derive(Clone, Default)]
struct Selection {
    start: usize,
    end: usize,
    pending: bool,
}

impl Selection {
    fn range(&self) -> Range<usize> {
        let start = self.start.min(self.end);
        let end = self.start.max(self.end);
        start..end
    }

}

pub struct MarkdownState {
    selection: Selection,
    focus_handle: FocusHandle,
}

impl MarkdownState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();

        cx.bind_keys([
            gpui::KeyBinding::new("ctrl-c", Copy, Some(MARKDOWN_CONTEXT)),
            gpui::KeyBinding::new("ctrl-insert", Copy, Some(MARKDOWN_CONTEXT)),
        ]);

        Self {
            selection: Selection::default(),
            focus_handle,
        }
    }
}

impl Focusable for MarkdownState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct RenderedBlock {
    layout: TextLayout,
    document_range: Range<usize>,
}

pub struct RenderedMarkdown {
    element: AnyElement,
    blocks: Vec<RenderedBlock>,
    document_text: String,
}

pub struct MarkdownElement {
    state: Entity<MarkdownState>,
    text: SharedString,
}

impl MarkdownElement {
    pub fn new(state: Entity<MarkdownState>, text: SharedString) -> Self {
        Self { state, text }
    }
}

impl IntoElement for MarkdownElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub struct PrepaintMarkdown {
    hitbox: Hitbox,
}

impl Element for MarkdownElement {
    type RequestLayoutState = RenderedMarkdown;
    type PrepaintState = PrepaintMarkdown;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut rendered = build_markdown(&self.text);
        let layout_id = rendered.element.request_layout(window, cx);
        (layout_id, rendered)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let focus_handle = self.state.read(cx).focus_handle.clone();
        window.set_focus_handle(&focus_handle, cx);
        rendered.element.prepaint(window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        PrepaintMarkdown { hitbox }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut key_context = KeyContext::default();
        key_context.add(MARKDOWN_CONTEXT);
        window.set_key_context(key_context);

        // Register copy action
        {
            let state = self.state.clone();
            let document_text = rendered.document_text.clone();
            window.on_action(
                TypeId::of::<Copy>(),
                move |_action, phase, _window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let selection = state.read(cx).selection.clone();
                        let range = selection.range();
                        if !range.is_empty() && range.end <= document_text.len() {
                            let text = document_text[range].to_string();
                            let connection = wayland::WaylandConnection::global(cx);
                            connection
                                .read(cx)
                                .send_command(wayland::Command::OfferText { text });
                        }
                    }
                },
            );
        }

        // Paint selection before text
        paint_selection(
            &self.state,
            rendered,
            bounds,
            window,
            cx,
        );

        // Paint the element tree (text)
        rendered.element.paint(window, cx);

        // Set cursor style
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);

        // Mouse down
        {
            let state = self.state.clone();
            let blocks: Vec<(TextLayout, Range<usize>)> = rendered
                .blocks
                .iter()
                .map(|b| (b.layout.clone(), b.document_range.clone()))
                .collect();
            let hitbox = prepaint.hitbox.clone();

            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if event.button != MouseButton::Left {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }

                let offset = offset_for_position(event.position, &blocks);
                let focus_handle = state.read(cx).focus_handle.clone();
                state.update(cx, |state, cx| {
                    state.selection = Selection {
                        start: offset,
                        end: offset,
                        pending: true,
                    };
                    cx.notify();
                });
                window.focus(&focus_handle, cx);
                window.prevent_default();
            });
        }

        // Mouse move (drag)
        {
            let state = self.state.clone();
            let blocks: Vec<(TextLayout, Range<usize>)> = rendered
                .blocks
                .iter()
                .map(|b| (b.layout.clone(), b.document_range.clone()))
                .collect();

            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }

                let is_pending = state.read(cx).selection.pending;
                if !is_pending {
                    return;
                }

                let offset = offset_for_position(event.position, &blocks);
                state.update(cx, |state, cx| {
                    state.selection.end = offset;
                    cx.notify();
                });
            });
        }

        // Mouse up
        {
            let state = self.state.clone();

            window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                let is_pending = state.read(cx).selection.pending;
                if !is_pending {
                    return;
                }

                state.update(cx, |state, cx| {
                    state.selection.pending = false;
                    cx.notify();
                });
            });
        }
    }
}

fn offset_for_position(
    position: Point<Pixels>,
    blocks: &[(TextLayout, Range<usize>)],
) -> usize {
    // TextLayout::index_for_position expects absolute window coordinates
    // and TextLayout::bounds() returns absolute window coordinates.

    let mut previous_block_end: Option<(Pixels, usize)> = None;

    for (layout, document_range) in blocks {
        let bounds = layout.bounds();
        if bounds.size.height == px(0.) {
            continue;
        }

        let block_top = bounds.origin.y;
        let block_bottom = block_top + bounds.size.height;

        // In the gap between the previous block and this one: snap to
        // the end of the previous block or the start of this one,
        // whichever is closer.
        if let Some((prev_bottom, prev_end_offset)) = previous_block_end {
            if position.y >= prev_bottom && position.y < block_top {
                let mid = prev_bottom + (block_top - prev_bottom) / 2.0;
                return if position.y < mid {
                    prev_end_offset
                } else {
                    document_range.start
                };
            }
        }

        if position.y >= block_top && position.y < block_bottom {
            let index = match layout.index_for_position(position) {
                Ok(index) | Err(index) => index,
            };
            return document_range.start + index.min(document_range.len());
        }

        previous_block_end = Some((block_bottom, document_range.end));
    }

    // If position is below all blocks, return end of document
    if let Some((_, last_range)) = blocks.last() {
        return last_range.end;
    }

    0
}

fn paint_selection(
    state: &Entity<MarkdownState>,
    rendered: &RenderedMarkdown,
    _bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let selection = state.read(cx).selection.clone();
    let range = selection.range();
    if range.is_empty() {
        return;
    }

    let selection_color = rgba(0x007ACC66);

    for block in &rendered.blocks {
        let block_start = block.document_range.start;
        let block_end = block.document_range.end;

        // Check if this block intersects the selection
        if range.end <= block_start || range.start >= block_end {
            continue;
        }

        let local_start = range.start.saturating_sub(block_start);
        let local_end = (range.end - block_start).min(block_end - block_start);

        let layout = &block.layout;
        let layout_bounds = layout.bounds();

        // position_for_index returns absolute window coordinates
        let start_pos = layout.position_for_index(local_start);
        let end_pos = layout.position_for_index(local_end);

        if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
            let line_height = layout.line_height();
            let right_edge = layout_bounds.origin.x + layout_bounds.size.width;
            let left_edge = layout_bounds.origin.x;

            if start_pos.y == end_pos.y {
                // Single line selection
                window.paint_quad(fill(
                    Bounds::from_corners(
                        start_pos,
                        point(end_pos.x, end_pos.y + line_height),
                    ),
                    selection_color,
                ));
            } else {
                // Multi-line selection: first line, middle, last line
                window.paint_quad(fill(
                    Bounds::from_corners(
                        start_pos,
                        point(right_edge, start_pos.y + line_height),
                    ),
                    selection_color,
                ));

                // Middle lines
                if end_pos.y > start_pos.y + line_height {
                    window.paint_quad(fill(
                        Bounds::from_corners(
                            point(left_edge, start_pos.y + line_height),
                            point(right_edge, end_pos.y),
                        ),
                        selection_color,
                    ));
                }

                // Last line: from left to end
                window.paint_quad(fill(
                    Bounds::from_corners(
                        point(left_edge, end_pos.y),
                        point(end_pos.x, end_pos.y + line_height),
                    ),
                    selection_color,
                ));
            }
        }
    }
}

fn build_markdown(text: &str) -> RenderedMarkdown {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(text, options);

    let mut elements: Vec<AnyElement> = Vec::new();
    let mut blocks: Vec<RenderedBlock> = Vec::new();
    let mut document_text = String::new();

    let mut inline_text = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut style_stack: Vec<HighlightStyle> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut list_item_index: u64 = 0;
    let mut heading_level: Option<u8> = None;
    let mut in_code_block = false;
    let mut in_blockquote = false;

    let current_style = |stack: &[HighlightStyle]| -> HighlightStyle {
        let mut combined = HighlightStyle::default();
        for style in stack {
            if style.font_weight.is_some() {
                combined.font_weight = style.font_weight;
            }
            if style.font_style.is_some() {
                combined.font_style = style.font_style;
            }
            if style.background_color.is_some() {
                combined.background_color = style.background_color;
            }
            if style.strikethrough.is_some() {
                combined.strikethrough = style.strikethrough;
            }
            if style.color.is_some() {
                combined.color = style.color;
            }
        }
        combined
    };

    let flush_block = |inline_text: &mut String,
                       highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
                       elements: &mut Vec<AnyElement>,
                       blocks: &mut Vec<RenderedBlock>,
                       document_text: &mut String,
                       heading_level: &mut Option<u8>,
                       in_code_block: bool,
                       in_blockquote: bool| {
        if inline_text.is_empty() {
            return;
        }

        let doc_start = document_text.len();
        document_text.push_str(inline_text);
        let doc_end = document_text.len();
        document_text.push('\n');

        let content: SharedString = inline_text.clone().into();
        let styled = if highlights.is_empty() {
            StyledText::new(content)
        } else {
            StyledText::new(content).with_highlights(std::mem::take(highlights))
        };

        let layout = styled.layout().clone();

        let element = if let Some(level) = heading_level.take() {
            let text_size = match level {
                1 => px(24.0),
                2 => px(20.0),
                3 => px(18.0),
                _ => px(16.0),
            };
            div()
                .text_size(text_size)
                .font_weight(FontWeight::BOLD)
                .mt_3()
                .mb_2()
                .child(styled)
        } else if in_code_block {
            div()
                .bg(rgba(0xFFFFFF12))
                .rounded_md()
                .px_3()
                .py_2()
                .mb_2()
                .font_family("Iosevka")
                .text_sm()
                .child(styled)
        } else if in_blockquote {
            div()
                .border_l_2()
                .border_color(rgba(0xFFFFFF44))
                .pl_3()
                .mb_2()
                .text_color(rgba(0xFFFFFFAA))
                .child(styled)
        } else {
            div().mb_2().child(styled)
        };

        elements.push(element.into_any_element());
        blocks.push(RenderedBlock {
            layout,
            document_range: doc_start..doc_end,
        });
        inline_text.clear();
        highlights.clear();
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    heading_level = Some(level as u8);
                }
                Tag::CodeBlock(_) => {
                    flush_block(
                        &mut inline_text,
                        &mut highlights,
                        &mut elements,
                        &mut blocks,
                        &mut document_text,
                        &mut heading_level,
                        in_code_block,
                        in_blockquote,
                    );
                    in_code_block = true;
                }
                Tag::BlockQuote(_) => {
                    in_blockquote = true;
                }
                Tag::List(start) => {
                    list_stack.push(start);
                    list_item_index = start.unwrap_or(1);
                }
                Tag::Item => {
                    flush_block(
                        &mut inline_text,
                        &mut highlights,
                        &mut elements,
                        &mut blocks,
                        &mut document_text,
                        &mut heading_level,
                        in_code_block,
                        in_blockquote,
                    );
                    let prefix = match list_stack.last() {
                        Some(Some(_)) => {
                            let prefix = format!("{list_item_index}. ");
                            list_item_index += 1;
                            prefix
                        }
                        _ => "• ".to_string(),
                    };
                    inline_text.push_str(&prefix);
                }
                Tag::Strong => {
                    style_stack.push(HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    });
                }
                Tag::Emphasis => {
                    style_stack.push(HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    });
                }
                Tag::Strikethrough => {
                    style_stack.push(HighlightStyle {
                        strikethrough: Some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: None,
                        }),
                        ..Default::default()
                    });
                }
                _ => {}
            },

            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
                    flush_block(
                        &mut inline_text,
                        &mut highlights,
                        &mut elements,
                        &mut blocks,
                        &mut document_text,
                        &mut heading_level,
                        in_code_block,
                        in_blockquote,
                    );
                }
                TagEnd::CodeBlock => {
                    flush_block(
                        &mut inline_text,
                        &mut highlights,
                        &mut elements,
                        &mut blocks,
                        &mut document_text,
                        &mut heading_level,
                        in_code_block,
                        in_blockquote,
                    );
                    in_code_block = false;
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                _ => {}
            },

            Event::Text(text) => {
                let start = inline_text.len();
                inline_text.push_str(&text);
                let end = inline_text.len();

                let style = current_style(&style_stack);
                if style != HighlightStyle::default() {
                    highlights.push((start..end, style));
                }
            }

            Event::Code(code) => {
                let start = inline_text.len();
                inline_text.push_str(&code);
                let end = inline_text.len();
                highlights.push((
                    start..end,
                    HighlightStyle {
                        background_color: Some(rgba(0xFFFFFF18).into()),
                        ..Default::default()
                    },
                ));
            }

            Event::SoftBreak => {
                inline_text.push(' ');
            }

            Event::HardBreak => {
                inline_text.push('\n');
            }

            _ => {}
        }
    }

    flush_block(
        &mut inline_text,
        &mut highlights,
        &mut elements,
        &mut blocks,
        &mut document_text,
        &mut heading_level,
        in_code_block,
        in_blockquote,
    );

    let container = div().children(elements).into_any_element();

    RenderedMarkdown {
        element: container,
        blocks,
        document_text,
    }
}
