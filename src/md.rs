use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn to_lines(md: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);

    let mut r = Renderer::default();
    for ev in parser {
        r.handle(ev);
    }
    r.flush_line();
    if r.lines.is_empty() {
        r.lines.push(Line::from(""));
    }
    r.lines
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    styles: Vec<Style>,
    heading_level: Option<HeadingLevel>,
    in_code_block: bool,
    list_stack: Vec<ListState>,
    blockquote_depth: usize,
    needs_item_bullet: Option<String>,
}

enum ListState {
    Bulleted,
    Ordered(u64),
}

impl Renderer {
    fn push_style(&mut self, s: Style) { self.styles.push(s); }
    fn pop_style(&mut self) { self.styles.pop(); }

    fn merged_style(&self) -> Style {
        self.styles.iter().fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn push_text(&mut self, text: String) {
        if let Some(bullet) = self.needs_item_bullet.take() {
            self.cur.push(Span::styled(
                bullet,
                Style::default().fg(Color::Rgb(170, 130, 80)),
            ));
        }
        let style = self.merged_style();
        self.cur.push(Span::styled(text, style));
    }

    fn flush_line(&mut self) {
        if !self.cur.is_empty() {
            let spans = std::mem::take(&mut self.cur);
            self.lines.push(Line::from(spans));
        }
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if !matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }

    fn prefix_blockquote(&mut self) {
        if self.blockquote_depth > 0 && self.cur.is_empty() && self.needs_item_bullet.is_none() {
            let prefix = "│ ".repeat(self.blockquote_depth);
            self.cur.push(Span::styled(
                prefix,
                Style::default().fg(Color::Rgb(120, 120, 120)),
            ));
        }
    }

    fn handle(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.handle_start(tag),
            Event::End(tag) => self.handle_end(tag),
            Event::Text(s) => {
                self.prefix_blockquote();
                if self.in_code_block {
                    // Code block text may span multiple lines — split and emit each.
                    let text = s.to_string();
                    let lines: Vec<&str> = text.split_inclusive('\n').collect();
                    for line in lines {
                        let ends_nl = line.ends_with('\n');
                        let body = line.trim_end_matches('\n');
                        let style = Style::default()
                            .fg(Color::Rgb(200, 200, 170))
                            .bg(Color::Rgb(30, 30, 36));
                        self.cur.push(Span::styled(body.to_string(), style));
                        if ends_nl {
                            self.flush_line();
                        }
                    }
                } else {
                    self.push_text(s.to_string());
                }
            }
            Event::Code(s) => {
                self.prefix_blockquote();
                let style = Style::default()
                    .fg(Color::Rgb(220, 180, 120))
                    .bg(Color::Rgb(40, 40, 48));
                self.cur.push(Span::styled(format!(" {} ", s), style));
            }
            Event::SoftBreak => {
                self.cur.push(Span::raw(" "));
            }
            Event::HardBreak => {
                self.flush_line();
                self.prefix_blockquote();
            }
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(60),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Event::Html(s) | Event::InlineHtml(s) => {
                self.prefix_blockquote();
                self.push_text(s.to_string());
            }
            Event::FootnoteReference(s) => {
                self.push_text(format!("[^{}]", s));
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.cur.push(Span::styled(
                    mark.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::InlineMath(s) | Event::DisplayMath(s) => {
                self.push_text(s.to_string());
            }
        }
    }

    fn handle_start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_line();
                self.prefix_blockquote();
            }
            Tag::Heading { level, .. } => {
                self.blank_line();
                self.heading_level = Some(level);
                let (color, marker) = match level {
                    HeadingLevel::H1 => (Color::Rgb(255, 210, 90), "█ "),
                    HeadingLevel::H2 => (Color::Rgb(120, 200, 255), "▌ "),
                    HeadingLevel::H3 => (Color::Rgb(160, 220, 140), "◆ "),
                    HeadingLevel::H4 => (Color::Rgb(200, 170, 230), "▸ "),
                    _ => (Color::Rgb(180, 180, 180), "· "),
                };
                self.cur.push(Span::styled(
                    marker.to_string(),
                    Style::default().fg(color),
                ));
                self.push_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
                self.push_style(Style::default().fg(Color::Rgb(170, 170, 170)));
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => s.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.lines.push(Line::from(Span::styled(
                    format!("┌─ {}", if lang.is_empty() { "code".to_string() } else { lang }),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Tag::List(first) => {
                self.flush_line();
                self.list_stack.push(match first {
                    Some(n) => ListState::Ordered(n),
                    None => ListState::Bulleted,
                });
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.list_stack.last_mut() {
                    Some(ListState::Bulleted) => format!("{indent}• "),
                    Some(ListState::Ordered(n)) => {
                        let out = format!("{indent}{}. ", n);
                        *n += 1;
                        out
                    }
                    None => "• ".to_string(),
                };
                self.needs_item_bullet = Some(bullet);
                self.prefix_blockquote();
            }
            Tag::Emphasis => {
                self.push_style(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.push_style(
                    Style::default()
                        .fg(Color::Rgb(120, 170, 255))
                        .add_modifier(Modifier::UNDERLINED),
                );
                // dest_url is referenced at end
                let _ = dest_url;
            }
            Tag::Image { dest_url, .. } => {
                self.push_text(format!("[image: {}]", dest_url));
            }
            Tag::Table(_) | Tag::TableHead | Tag::TableRow => {
                self.flush_line();
            }
            Tag::TableCell => {
                self.cur.push(Span::styled(
                    " │ ",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_line();
                self.cur.push(Span::styled(
                    format!("[^{}]: ", label),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Tag::MetadataBlock(_) | Tag::HtmlBlock | Tag::DefinitionList | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {
                self.flush_line();
            }
            Tag::Subscript | Tag::Superscript => {
                self.push_style(Style::default().fg(Color::DarkGray));
            }
        }
    }

    fn handle_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            TagEnd::Heading(level) => {
                self.pop_style();
                let lvl = self.heading_level.take();
                self.flush_line();
                let _ = lvl;
                let rule = match level {
                    HeadingLevel::H1 => Some(('━', Color::Rgb(255, 210, 90))),
                    HeadingLevel::H2 => Some(('─', Color::Rgb(120, 200, 255))),
                    _ => None,
                };
                if let Some((ch, col)) = rule {
                    let s: String = std::iter::repeat(ch).take(60).collect();
                    self.lines.push(Line::from(Span::styled(
                        s,
                        Style::default().fg(col),
                    )));
                }
                self.lines.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.pop_style();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.lines.push(Line::from(""));
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
                self.lines.push(Line::from(Span::styled(
                    "└─".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
                self.lines.push(Line::from(""));
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.needs_item_bullet = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link
            | TagEnd::Subscript | TagEnd::Superscript => self.pop_style(),
            TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow => self.flush_line(),
            TagEnd::TableCell => {}
            TagEnd::Image => {}
            TagEnd::FootnoteDefinition => self.flush_line(),
            TagEnd::MetadataBlock(_) | TagEnd::HtmlBlock | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle | TagEnd::DefinitionListDefinition => self.flush_line(),
        }
    }
}
