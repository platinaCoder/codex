//! Compose the owned transcript above the composer and route their selection gestures.
//! Reserve a cleared row below activity and previews, immediately above the composer.
//! Slash suggestions overlay already-painted rows so opening or closing them leaves transcript
//! geometry unchanged. Turn tips remain outside transcript history; their painted rows are selectable.
//! Plain Enter returns an empty composer to latest after transcript interactions and prompt editing.

use super::turn_tips::TipSurface;
use super::*;
use crate::history_cell::HistoryRenderMode;
use crate::keymap::KeymapContext;
use crate::keymap::bindings_for_action;
use crate::keymap::configured_binding_for_action;
use crate::keymap::keymap_action_ids;
use crate::motion::MotionMode;
use crate::pager_overlay::TranscriptHistoryState;
use crate::transcript_view::ViewAction;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Widget;

const PAIRED_WORKSPACE_MIN_WIDTH: u16 = 48;
const PAIRED_WORKSPACE_MIN_HEIGHT: u16 = 10;
const PAIRED_WORKSPACE_OUTER_MARGIN: u16 = 1;
const PAIRED_WORKSPACE_GAP: u16 = 2;
const INFERENCE_WORKSPACE_GAP: u16 = 1;
const INFERENCE_PANE_MIN_HEIGHT: u16 = 8;
const PRIMARY_WORKSPACE_MIN_HEIGHT: u16 = 7;

fn paired_pane_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

impl App {
    /// Copy draft selections before key shortcuts, or after mouse layout has been refreshed.
    pub(super) fn handle_composer_copy_event(
        &mut self,
        tui: &mut tui::Tui,
        event: &TuiEvent,
        copy: impl FnOnce(&mut tui::Tui, &str) -> Result<crate::clipboard_copy::CopyStatus, String>,
    ) -> bool {
        if tui.is_owned_screen()
            && self.overlay.is_none()
            && !self.transcript_view.has_active_interaction()
            && let Some((char_count, result)) = self
                .chat_widget
                .copy_composer_selection(event, |text| copy(tui, text))
        {
            self.cancel_pending_key_chord();
            self.transcript_view.show_copy_feedback(&result, char_count);
            tui.frame_requester().schedule_frame();
            return true;
        }
        false
    }

    /// Publish a completed composer selection with the transcript/footer copy policy.
    fn handle_composer_selection_release(
        &mut self,
        tui: &mut tui::Tui,
        event: crossterm::event::MouseEvent,
    ) -> bool {
        let selection_copy = crate::tui::SelectionCopy::on_select(
            self.local_settings
                .copy_on_select(&codex_terminal_detection::terminal_info()),
            self.right_click_paste_environment.primary,
            // Editable selections remain visible so the next input can replace them.
            /*clear_selection*/
            false,
        );
        if !selection_copy.copies_clipboard() && !selection_copy.publishes_primary() {
            return false;
        }
        let Some((char_count, result)) = self.chat_widget.copy_composer_selection_on_release(
            event,
            selection_copy,
            |text, selection_copy| {
                tui.copy_selection(
                    text,
                    crate::clipboard_copy::CopyFormat::PlainText,
                    selection_copy,
                    text,
                )
            },
        ) else {
            return false;
        };
        if let Some(result) = result {
            self.transcript_view.show_copy_feedback(&result, char_count);
        }
        true
    }

    fn sync_owned_transcript(&mut self, width: u16) -> bool {
        let chat_widget = &self.chat_widget;
        let transcript_width = chat_widget.history_wrap_width(width);
        let view = &mut self.transcript_view;
        view.image_protocol = chat_widget.pet_image_support().protocol().filter(|_| {
            !self.app_server_target.uses_remote_workspace()
                && crate::projectless::has_only_local_environments(&self.environment_manager)
        });
        view.mouse_scroll_speed = self.local_settings.tui.mouse_scroll_speed.unwrap_or(1.0);
        view.primary_selection = self.right_click_paste_environment.primary;
        view.copy_on_select = self
            .local_settings
            .copy_on_select(&codex_terminal_detection::terminal_info());
        view.copy_on_select_clear_selection =
            self.local_settings.tui.copy_on_select_clear_selection;
        view.set_keymap_bindings(&self.keymap);
        view.set_presentation(view.is_detailed(), chat_widget.history_render_mode());
        let active_key = chat_widget.active_cell_transcript_key();
        let detailed = view.is_detailed();
        let active_ids = chat_widget.active_activity_ids();
        let expanded = view.sync_live_activity(&self.transcript_cells, active_ids)
            && chat_widget.history_render_mode() == HistoryRenderMode::Rich;
        let search_changed = view.sync_search_live_tail(transcript_width, active_key, |width| {
            chat_widget.active_cell_transcript_hyperlink_lines(width)
        });
        let changed = if detailed {
            view.sync_live_tail(transcript_width, active_key, |width| {
                chat_widget.active_cell_transcript_hyperlink_lines(width)
            })
        } else {
            view.sync_live_activity_tail(transcript_width, active_key, expanded, |width| {
                chat_widget.active_cell_owned_transcript_lines(width, expanded)
            })
        };
        changed || search_changed
    }

    pub(super) fn render_owned_transcript(
        &mut self,
        tui: &mut tui::Tui,
        screen_size: Size,
    ) -> Result<Rect> {
        self.refresh_ticket_strip();
        let infer_render = self.inference_render_data();
        let decorated_workspace = infer_render.is_some() || self.paired_main_and_side().is_some();
        let mut workspace = if decorated_workspace {
            Rect::new(
                PAIRED_WORKSPACE_OUTER_MARGIN,
                PAIRED_WORKSPACE_OUTER_MARGIN,
                screen_size
                    .width
                    .saturating_sub(PAIRED_WORKSPACE_OUTER_MARGIN * 2),
                screen_size
                    .height
                    .saturating_sub(PAIRED_WORKSPACE_OUTER_MARGIN * 2),
            )
        } else {
            Rect::new(
                /*x*/ 0,
                /*y*/ 0,
                screen_size.width,
                screen_size.height,
            )
        };
        let ticket_height = self.ticket_strip_height().min(workspace.height);
        let ticket_area = (ticket_height > 0)
            .then(|| Rect::new(workspace.x, workspace.y, workspace.width, ticket_height));
        if ticket_height > 0 {
            let consumed = ticket_height + u16::from(workspace.height > ticket_height);
            workspace.y = workspace.y.saturating_add(consumed);
            workspace.height = workspace.height.saturating_sub(consumed);
        }
        let (primary_workspace, infer_area) = if infer_render.is_some()
            && workspace.height > PRIMARY_WORKSPACE_MIN_HEIGHT + INFERENCE_WORKSPACE_GAP
        {
            let infer_height = (workspace.height * 2 / 5)
                .max(INFERENCE_PANE_MIN_HEIGHT)
                .min(
                    workspace
                        .height
                        .saturating_sub(PRIMARY_WORKSPACE_MIN_HEIGHT + INFERENCE_WORKSPACE_GAP),
                );
            let primary_height = workspace
                .height
                .saturating_sub(infer_height + INFERENCE_WORKSPACE_GAP);
            (
                Rect::new(workspace.x, workspace.y, workspace.width, primary_height),
                Some(Rect::new(
                    workspace.x,
                    workspace.y + primary_height + INFERENCE_WORKSPACE_GAP,
                    workspace.width,
                    infer_height,
                )),
            )
        } else {
            (workspace, None)
        };
        let infer = infer_area.zip(infer_render);
        if let Some((main_thread_id, side_thread_id)) = self.paired_main_and_side()
            && primary_workspace.width >= PAIRED_WORKSPACE_MIN_WIDTH
            && primary_workspace.height >= PAIRED_WORKSPACE_MIN_HEIGHT
        {
            let pane_width = primary_workspace.width.saturating_sub(PAIRED_WORKSPACE_GAP);
            let left_width = pane_width / 2;
            let left = Rect::new(
                primary_workspace.x,
                primary_workspace.y,
                left_width,
                primary_workspace.height,
            );
            let right = Rect::new(
                primary_workspace.x + left_width + PAIRED_WORKSPACE_GAP,
                primary_workspace.y,
                pane_width - left_width,
                primary_workspace.height,
            );
            let active_is_main = self.current_displayed_thread_id() == Some(main_thread_id);
            let chat_focused = !self.inference_focused();
            let (active_area, inactive_area, inactive_id, active_title, inactive_title) =
                if active_is_main {
                    (
                        left,
                        right,
                        side_thread_id,
                        if chat_focused {
                            "MAIN • focused"
                        } else {
                            "MAIN"
                        }
                        .to_string(),
                        "SIDE".to_string(),
                    )
                } else {
                    (
                        right,
                        left,
                        main_thread_id,
                        if chat_focused {
                            "SIDE • focused"
                        } else {
                            "SIDE"
                        }
                        .to_string(),
                        "MAIN".to_string(),
                    )
                };
            let mut inactive_state = self.paired_pane_states.remove(&inactive_id);
            let active_inner = paired_pane_inner(active_area);
            let result = self.render_owned_transcript_in_area(
                tui,
                screen_size,
                active_inner,
                Some(PairedPaneRender {
                    state: inactive_state.as_mut(),
                    active_area,
                    inactive_area,
                    active_title,
                    inactive_title,
                }),
                ticket_area,
                infer,
            );
            if let Some(inactive_state) = inactive_state {
                self.paired_pane_states.insert(inactive_id, inactive_state);
            }
            return result;
        }
        self.render_owned_transcript_in_area(
            tui,
            screen_size,
            primary_workspace,
            None,
            ticket_area,
            infer,
        )
    }

    fn render_owned_transcript_in_area(
        &mut self,
        tui: &mut tui::Tui,
        screen_size: Size,
        pane: Rect,
        mut paired: Option<PairedPaneRender<'_>>,
        ticket_area: Option<Rect>,
        infer: Option<(Rect, inference::InferPaneRenderData)>,
    ) -> Result<Rect> {
        self.chat_widget.sync_warnings(&self.transcript_cells);
        let motion = MotionMode::from_animations_enabled(
            self.local_settings.tui.animations && self.local_settings.tui.effects.shimmer,
        );
        let composer = self.first_screen_composer();
        let latest_navigation = if self.enter_returns_to_latest() {
            "enter/esc latest"
        } else {
            "esc latest"
        };
        self.sync_owned_transcript(pane.width);
        let transcript_width = self.chat_widget.history_wrap_width(pane.width);
        let composer_hint = if self
            .chat_widget
            .rendered_selection
            .borrow()
            .view
            .has_active_interaction()
        {
            Some(
                ratatui::text::Line::from("Selecting text · esc cancel")
                    .dim()
                    .into(),
            )
        } else {
            self.composer_hint(transcript_width)
        };
        let now = Instant::now();
        let turn_tip = self.turn_tip(transcript_width, now, &tui.frame_requester());
        let working_tip = turn_tip
            .as_ref()
            .filter(|(surface, _)| *surface == TipSurface::Working)
            .map(|(_, tip)| tip);
        let completion_tip = turn_tip
            .as_ref()
            .filter(|(surface, _)| *surface == TipSurface::Completion)
            .map(|(_, tip)| tip);
        let mut composer_gap = (!self.chat_widget.has_active_view()
            && !self.chat_widget.is_external_writer_view())
        .then(crate::bottom_pane::ComposerGap::default);
        let mut prompt_footer = self.prompt_navigation_footer(pane.width.saturating_sub(/*rhs*/ 2));
        let chat_widget = &self.chat_widget;
        let ticket_ui = &self.ticket_ui;
        let view = &mut self.transcript_view;
        let active_key = chat_widget.active_cell_transcript_key();
        view.sync_history_tail(&self.transcript_cells);
        view.prepare_width(transcript_width);
        if view.advance_search(&self.transcript_cells) {
            tui.frame_requester().schedule_frame();
        }
        let footer_area = crate::bottom_pane::inset_footer_hint_area(Rect::new(
            pane.x, pane.y, pane.width, /*height*/ 1,
        ));
        let footer = view.footer_with_navigation(footer_area.width, motion, latest_navigation);
        // Cap the whole composer (including padding and hints) at two-thirds of the screen,
        // but allow at least 8 rows on small screens, without exceeding the screen itself.
        let max_composer_height = ((u32::from(pane.height) * 2 / 3) as u16)
            .max(8)
            .min(pane.height);
        let bottom =
            chat_widget.bottom_pane_renderable(crate::bottom_pane::ComposerRenderOptions {
                max_height: Some(max_composer_height),
                footer: prompt_footer.as_ref().or(footer.as_ref()),
                command_popup_placement: if view.has_active_interaction() {
                    crate::bottom_pane::CommandPopupPlacement::Hidden
                } else {
                    crate::bottom_pane::CommandPopupPlacement::Overlay
                },
                composer_gap: composer_gap.as_ref(),
                working_tip,
                ..Default::default()
            });
        let dashboard_visible = chat_widget
            .selected_index_for_present_view(AGENTS_OVERVIEW_VIEW_ID)
            .is_some();
        let bottom_height = if dashboard_visible {
            pane.height
        } else {
            bottom.desired_height(pane.width).min(pane.height)
        };
        drop(bottom);
        let available = pane.height.saturating_sub(bottom_height);
        let mut bottom_area = Rect::new(
            pane.x,
            pane.y + pane.height.saturating_sub(bottom_height),
            pane.width,
            bottom_height,
        );
        let mut rendered_cursor = None;
        let mut footer_height_changed = false;
        let mut feedback_tick = None;
        let mut blossom_tick = None;
        let mut transcript_bottom = available.saturating_sub(u16::from(composer_gap.is_none()));
        if let Some(paired) = paired.as_mut() {
            let width = paired_pane_inner(paired.inactive_area).width;
            if let Some(state) = paired.state.as_mut() {
                state
                    .transcript_view
                    .sync_history_tail(&state.transcript_cells);
                state.transcript_view.prepare_width(width);
            }
        }
        tui.draw(screen_size.height, |frame| {
            if let Some(area) = ticket_area {
                ticket_ui.render(area, frame.buffer);
            }
            if let Some(paired) = paired.as_mut() {
                let area = paired.inactive_area;
                ratatui::widgets::Clear.render(area, frame.buffer);
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(ratatui::style::Style::default().dim())
                    .title(ratatui::text::Line::from(format!(" {} ", paired.inactive_title)).dim())
                    .render(area, frame.buffer);
                let inner = paired_pane_inner(area);
                if let Some(state) = paired.state.as_mut() {
                    state
                        .transcript_view
                        .render(inner, frame.buffer, &state.transcript_cells);
                } else {
                    ratatui::widgets::Paragraph::new(
                        "Persistent chat is ready. Loading its transcript…",
                    )
                    .dim()
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .render(
                        Rect::new(
                            inner.x + 1,
                            inner.y + 1,
                            inner.width.saturating_sub(2),
                            inner.height.saturating_sub(2),
                        ),
                        frame.buffer,
                    );
                }
                ratatui::widgets::Clear.render(paired.active_area, frame.buffer);
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(
                        ratatui::text::Line::from(format!(
                            " {} · ctrl+h/j/k/l focus ",
                            paired.active_title
                        ))
                        .bold(),
                    )
                    .render(paired.active_area, frame.buffer);
            }
            chat_widget.rendered_selection.borrow_mut().areas.clear();
            ratatui::widgets::Clear.render(
                Rect::new(pane.x, pane.y, pane.width, available),
                frame.buffer,
            );
            let mut completion_tip_area = view.render_with_turn_tip_space(
                Rect::new(pane.x, pane.y, transcript_width, transcript_bottom),
                frame.buffer,
                &self.transcript_cells,
                completion_tip,
            );
            if let Some(gap) = composer_gap.as_mut() {
                gap.needs_separator = available > 1
                    // Keep the tip under the pointer when selection hints or copy feedback appear.
                    && turn_tip.is_none()
                    && chat_widget.no_modal_or_popup_active()
                    && view.composer_gap_has_content(transcript_width, composer_hint.as_ref(), now);
            }
            // Rendering resolves whether new activity is still hidden. Paint that result in
            // this frame so a revision change cannot flash a stale activity hint.
            let mut footer =
                view.footer_with_navigation(footer_area.width, motion, latest_navigation);
            if let Some(footer) = prompt_footer
                .as_mut()
                .or(footer.as_mut())
                .filter(|footer| footer.is_interactive)
                && let Some(items) = self
                    .key_chord_matcher
                    .pending_hint_items(&self.keymap.chords, footer_area.width)
                && let Some(progress) = footer.text.lines.last_mut()
            {
                *progress = crate::bottom_pane::footer_hint_items_line(&items);
            }
            let bottom =
                chat_widget.bottom_pane_renderable(crate::bottom_pane::ComposerRenderOptions {
                    max_height: Some(max_composer_height),
                    footer: prompt_footer.as_ref().or(footer.as_ref()),
                    command_popup_placement: if view.has_active_interaction() {
                        crate::bottom_pane::CommandPopupPlacement::Hidden
                    } else {
                        crate::bottom_pane::CommandPopupPlacement::Overlay
                    },
                    composer_gap: composer_gap.as_ref(),
                    working_tip,
                    ..Default::default()
                });
            footer_height_changed = !dashboard_visible
                && bottom.desired_height(pane.width).min(pane.height) != bottom_height;
            if footer_height_changed && composer_gap.as_ref().is_some_and(|gap| gap.needs_separator)
            {
                bottom_area.height = bottom.desired_height(pane.width).min(pane.height);
                bottom_area.y = pane.y + pane.height.saturating_sub(bottom_area.height);
                // Resolve controls with the compact viewport first, then make room for
                // their separator. Resizing must not preserve a stale return control.
                ratatui::widgets::Clear.render(bottom_area, frame.buffer);
                transcript_bottom = bottom_area.y.saturating_sub(pane.y);
                completion_tip_area = view.render_with_turn_tip_space(
                    Rect::new(pane.x, pane.y, transcript_width, transcript_bottom),
                    frame.buffer,
                    &self.transcript_cells,
                    completion_tip,
                );
                footer_height_changed = false;
            }
            blossom_tick = chat_widget
                .empty_state_animation
                .borrow_mut()
                .render_first_screen(
                    Rect {
                        width: pane.width,
                        ..view.remaining_area()
                    },
                    frame.buffer,
                    composer,
                    MotionMode::from_animations_enabled(self.local_settings.tui.animations),
                );
            bottom.render(bottom_area, frame.buffer);
            if let (Some(tip), Some(area)) = (completion_tip, completion_tip_area) {
                tip.render(area, frame.buffer);
            }
            let follow_area = if let Some(gap) = composer_gap.as_ref() {
                Some(Rect {
                    width: transcript_width,
                    ..gap.area.get()
                })
            } else {
                (available > 0).then(|| {
                    Rect::new(
                        pane.x,
                        pane.y + available - 1,
                        transcript_width,
                        /*height*/ 1,
                    )
                })
            }
            .filter(|_| chat_widget.no_modal_or_popup_active());
            feedback_tick =
                view.render_composer_gap(follow_area, composer_hint.as_ref(), frame.buffer, now);
            if let Some((_, tip)) = &turn_tip
                && let Some(area) = tip.rendered.get()
            {
                chat_widget
                    .rendered_selection
                    .borrow_mut()
                    .register(area, /*invalidate*/ false);
            }
            chat_widget
                .rendered_selection
                .borrow_mut()
                .render(frame.buffer);
            chat_widget.note_rendered_width(pane.width);
            let dialog = chat_widget.centered_dialog();
            let (foreground, foreground_area): (&dyn Renderable, Rect) =
                if let Some(dialog) = &dialog {
                    let area = Rect::new(pane.x, pane.y, pane.width, pane.height);
                    dialog.render(area, frame.buffer);
                    (dialog, area)
                } else {
                    (&bottom, bottom_area)
                };
            rendered_cursor = foreground.cursor_pos(foreground_area);
            if let Some(position) = rendered_cursor {
                frame.set_cursor_style(foreground.cursor_style(foreground_area));
                frame.set_cursor_position(position);
            }
            if let Some((infer_area, infer_data)) = infer.as_ref()
                && let Some(position) =
                    super::inference::render_inference_pane(infer_data, *infer_area, frame.buffer)
            {
                frame.set_cursor_position(position);
            }
            if chat_widget.no_modal_or_popup_active() {
                frame.transcript_images = view.image_placements();
            }
        })?;
        if let Some((surface, tip)) = &turn_tip
            && tip.rendered.get().is_some()
        {
            self.turn_tips.acknowledge(*surface);
        }
        if footer_height_changed {
            tui.frame_requester().schedule_frame();
        }
        if let Some(delay) = feedback_tick {
            tui.frame_requester().schedule_frame_in(delay);
        }
        if let Some(delay) = blossom_tick {
            tui.frame_requester().schedule_frame_in(delay);
        }
        let animating =
            view.is_following() && active_key.is_some_and(|key| key.animation_tick.is_some());
        let loading = view.is_loading_history() && motion == MotionMode::Animated;
        if animating || loading {
            tui.frame_requester()
                .schedule_frame_in(Duration::from_millis(/*millis*/ 50));
        }
        self.refresh_link_hover(tui)?;
        Ok(bottom_area)
    }

    /// Keep modal input ownership while allowing selection and copying in the visible transcript.
    pub(super) fn handle_owned_transcript_event(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        event: &TuiEvent,
    ) -> Result<bool> {
        let has_modal = self.chat_widget.has_active_modal();
        let modal_transcript_mouse = has_modal
            && matches!(event, TuiEvent::Mouse(mouse)
                if self.chat_widget.centered_dialog().is_none()
                    || matches!(mouse.kind, crossterm::event::MouseEventKind::ScrollUp
                        | crossterm::event::MouseEventKind::ScrollDown));
        let modal_transcript_draw = has_modal
            && self.chat_widget.centered_dialog().is_none()
            && matches!(event, TuiEvent::Draw);
        let modal_transcript_event = modal_transcript_mouse
            || modal_transcript_draw
            || (has_modal
                && matches!(event, TuiEvent::Key(key)
                    if crate::text_selection::is_copy_key(*key)
                        && self.transcript_view.owns_interaction_key(*key)));
        if !tui.is_owned_screen()
            || matches!(event, TuiEvent::FocusLost | TuiEvent::Resume)
            || self.overlay.is_some()
            || (!self.chat_widget.no_modal_or_popup_active() && !modal_transcript_event)
        {
            self.transcript_view.end_drag();
        }
        if !tui.is_owned_screen()
            || matches!(event, TuiEvent::FocusLost | TuiEvent::Resume)
            || self.overlay.is_some()
        {
            self.chat_widget.end_composer_drag();
        }
        if !tui.is_owned_screen() || self.overlay.is_some() {
            return Ok(false);
        }
        if matches!(event, TuiEvent::FocusLost) {
            self.chat_widget
                .empty_state_animation
                .borrow_mut()
                .cancel_replay();
            // Show the static, faded decoration immediately when the terminal loses focus.
            tui.frame_requester().schedule_frame();
        }
        if self.transcript_view.history == TranscriptHistoryState::Idle {
            self.transcript_view.history = if self.scrollback_has_older_history {
                TranscriptHistoryState::Partial
            } else {
                TranscriptHistoryState::Complete
            };
        }
        if matches!(event, TuiEvent::Draw) {
            if self.transcript_view.tick_selection(&self.transcript_cells) {
                tui.frame_requester()
                    .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
            }
            return Ok(false);
        }
        if let TuiEvent::Mouse(mouse) = event {
            let composer_ready = self.chat_widget.prepare_composer_mouse(*mouse);
            if mouse.kind != crossterm::event::MouseEventKind::Moved {
                let size = tui.prepare_draw_size()?;
                self.render_owned_transcript(tui, size)?;
            }
            if self.chat_widget.no_modal_or_popup_active()
                && self
                    .chat_widget
                    .empty_state_animation
                    .borrow_mut()
                    .handle_mouse(*mouse)
            {
                tui.frame_requester().schedule_frame();
                return Ok(true);
            }
            if composer_ready
                && self.handle_composer_copy_event(tui, event, |tui, text| {
                    tui.copy_transcript_selection(
                        text,
                        crate::clipboard_copy::CopyFormat::PlainText,
                    )
                })
            {
                return Ok(true);
            }
            if composer_ready && self.chat_widget.handle_composer_mouse(*mouse) {
                self.handle_composer_selection_release(tui, *mouse);
                if !matches!(
                    mouse.kind,
                    crossterm::event::MouseEventKind::ScrollUp
                        | crossterm::event::MouseEventKind::ScrollDown
                ) {
                    self.transcript_view.end_selection(&self.transcript_cells);
                    self.transcript_view.cancel_search();
                    self.transcript_view.clear_activity_focus();
                }
                tui.frame_requester().schedule_frame();
                return Ok(true);
            }
        }
        if !self.chat_widget.no_modal_or_popup_active() {
            self.chat_widget.end_composer_drag();
            if !modal_transcript_event {
                return Ok(false);
            }
        }
        // Raw pastes can arrive as thousands of Char/Enter/Tab events. Honor the composer's
        // paste batching instead of rendering the entire screen before each buffered key.
        // Keys owned by the transcript and non-text keys still need fresh geometry immediately.
        if let TuiEvent::Key(key) = event
            && key.kind != KeyEventKind::Release
            && (!self.chat_widget.is_in_paste_burst()
                || self.transcript_view.is_search_editing()
                || self.transcript_view.is_activity_focused()
                || self.transcript_view.owns_interaction_key(*key)
                || self.backtrack.overlay_preview_active
                || self.keymap.app.focus_activity.is_pressed(*key)
                || self.keymap.app.find_transcript.is_pressed(*key)
                || match key.code {
                    KeyCode::Char(_) => {
                        crate::key_hint::has_ctrl_or_alt(key.modifiers)
                            || key.modifiers.intersects(
                                KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
                            )
                    }
                    KeyCode::Enter => {
                        !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
                    }
                    KeyCode::Tab => key.modifiers != KeyModifiers::NONE,
                    _ => true,
                })
        {
            let size = tui.prepare_draw_size()?;
            self.render_owned_transcript(tui, size)?;
        }
        if matches!(event, TuiEvent::Key(_))
            || matches!(event, TuiEvent::Mouse(mouse) if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)))
        {
            self.chat_widget.end_composer_drag();
        }
        // Read-only Escape belongs to session navigation, even when the transcript is scrolled.
        // Search and selection still consume Escape first to dismiss their interaction.
        if let TuiEvent::Key(key) = event
            && !self.transcript_view.has_active_interaction()
            && self.chat_widget.is_external_writer_view()
            && crate::key_hint::plain(KeyCode::Esc).is_press(*key)
        {
            return Ok(false);
        }
        // Visible shortcut help owns Escape before returning a paused viewport to latest.
        // A transcript search or selection still owns Escape while it replaces the help footer.
        if let TuiEvent::Key(key) = event
            && !self.transcript_view.has_active_interaction()
            && self.chat_widget.shortcut_overlay_visible()
            && crate::key_hint::plain(KeyCode::Esc).is_press(*key)
        {
            return Ok(false);
        }
        if let TuiEvent::Key(key) = event
            && !self.transcript_view.has_active_interaction()
            && self.chat_widget.should_handle_vim_insert_escape(*key)
        {
            return Ok(false);
        }
        // Prompt preview owns its navigation before ordinary Escape-to-latest scrolling.
        // Selection and search retain their existing priority and keep the prompt untouched.
        if self.backtrack.overlay_preview_active
            && !self.transcript_view.has_active_interaction()
            && self.handle_owned_backtrack_event(tui, event)?
        {
            self.request_owned_history(tui, app_server);
            return Ok(true);
        }
        // Capture the origin before the first Escape returns a paused viewport to latest.
        if !self.backtrack.overlay_preview_active
            && !self.transcript_view.has_active_interaction()
            && !self.reconnect.offline
            && let TuiEvent::Key(key) = event
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && key.kind == KeyEventKind::Press
            && self.should_handle_backtrack_esc(*key)
        {
            if !self.handle_owned_backtrack_event(tui, event)? {
                self.handle_backtrack_esc_key(tui);
                if !self.backtrack.overlay_preview_active {
                    self.transcript_view.jump_to_latest();
                }
            }
            tui.frame_requester().schedule_frame();
            return Ok(true);
        }
        if let TuiEvent::Key(key) = event
            && !self.transcript_view.has_active_interaction()
            && !self.backtrack.overlay_preview_active
            && self.transcript_view.is_detailed()
            && self.keymap.pager.close_transcript.is_pressed(*key)
        {
            self.close_transcript_overlay(tui);
            return Ok(true);
        }
        let empty_enter_returns_to_latest = matches!(
            event,
            TuiEvent::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            })
        ) && self.enter_returns_to_latest()
            && self.transcript_view.can_return_to_latest();
        if let TuiEvent::Key(key) = event
            && !self.transcript_view.has_active_interaction()
            && keymap_action_ids().any(|action| {
                action.context != KeymapContext::Pager
                    && !matches!(action.action, "find_transcript" | "focus_activity")
                    && self.active_keymap_contexts().contains_action(action)
                    && !(empty_enter_returns_to_latest
                        && action.context == KeymapContext::Composer
                        && action.action == "submit")
                    && ((key.modifiers.is_empty()
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown))
                        || crate::transcript_view::JumpTarget::from_key(*key).is_some()
                        || configured_binding_for_action(&self.local_settings.tui.keymap, action)
                            .is_some_and(std::option::Option::is_some))
                    && bindings_for_action(
                        &self.keymap,
                        action.context.config_name(),
                        action.action,
                    )
                    .is_some_and(|bindings| bindings.is_pressed(*key))
            })
        {
            return Ok(false);
        }
        let action = match event {
            TuiEvent::Key(key)
                if !self.transcript_view.is_search_editing()
                    && self.keymap.app.find_transcript.is_pressed(*key) =>
            {
                self.transcript_view.begin_search();
                Some(ViewAction::Changed)
            }
            TuiEvent::Key(key) => self
                .transcript_view
                .handle_key(*key, &self.transcript_cells),
            TuiEvent::Mouse(mouse) if modal_transcript_mouse => self
                .transcript_view
                .handle_selection_mouse(*mouse, &self.transcript_cells),
            TuiEvent::Mouse(mouse) => self
                .transcript_view
                .handle_mouse(*mouse, &self.transcript_cells),
            TuiEvent::Paste(text) => {
                self.transcript_view.end_selection(&self.transcript_cells);
                if self.transcript_view.paste_search(text) {
                    Some(ViewAction::Changed)
                } else {
                    self.transcript_view.clear_activity_focus();
                    None
                }
            }
            TuiEvent::Draw
            | TuiEvent::Resume
            | TuiEvent::Resize(_)
            | TuiEvent::FocusGained
            | TuiEvent::FocusLost => None,
        };
        let Some(action) = action else {
            if empty_enter_returns_to_latest {
                self.transcript_view.jump_to_latest();
                tui.frame_requester().schedule_frame();
                return Ok(true);
            }
            if self.reconnect.offline {
                if let TuiEvent::Key(key) = event
                    && self.transcript_view.is_detailed()
                    && self.keymap.pager.close_transcript.is_pressed(*key)
                {
                    self.close_transcript_overlay(tui);
                    return Ok(true);
                }
                return Ok(false);
            }
            return self.handle_owned_backtrack_event(tui, event);
        };
        if let Some((characters, result)) = self.transcript_view.copy_action(
            tui,
            &self.transcript_cells,
            &action,
            crate::clipboard_copy::CopyFormat::Markdown,
        ) {
            self.transcript_view.show_copy_feedback(&result, characters);
        } else if let ViewAction::OpenLink(url) = action {
            self.open_url_in_browser(url);
        }
        self.request_owned_history(tui, app_server);
        tui.frame_requester().schedule_frame();
        Ok(true)
    }

    /// Keep the navigation hint and input guard aligned, including pending paste and attachments.
    fn enter_returns_to_latest(&self) -> bool {
        self.chat_widget.composer_is_empty()
            && self.chat_widget.no_modal_or_popup_active()
            && !self.backtrack.primed
            && !self.backtrack.overlay_preview_active
    }

    pub(super) fn request_owned_history(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
    ) {
        if !self.scrollback_has_older_history {
            return;
        }
        let browsing_needs_history = self.browsing_needs_history();
        let view = &mut self.transcript_view;
        if !browsing_needs_history
            && !view.needs_history(&self.transcript_cells)
            && view.history != TranscriptHistoryState::LoadingBeginning
        {
            return;
        }
        if let Some(thread_id) = self.chat_widget.thread_id()
            && app_server.has_older_history(thread_id)
            && self.request_older_history_page(app_server, thread_id)
        {
            if self.transcript_view.history != TranscriptHistoryState::LoadingBeginning {
                self.transcript_view.history = TranscriptHistoryState::LoadingOlder;
            }
            tui.frame_requester().schedule_frame();
        }
    }
}

#[cfg(test)]
#[path = "owned_transcript_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "empty_state_animation_tests.rs"]
mod empty_state_animation_tests;

#[cfg(test)]
#[path = "owned_transcript_input_tests.rs"]
mod input_tests;

#[cfg(test)]
#[path = "warning_notice_tests.rs"]
mod warning_notice_tests;

#[cfg(test)]
#[path = "owned_transcript_follow_tests.rs"]
mod follow_tests;
