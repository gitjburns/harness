//! Chat loop: input events, the tool-calling turn, and session persistence.
//!
//! A turn starts with a user message and alternates between streaming a response and
//! handling its tool calls (classify or ask, then run in the turn's REPL). It ends
//! when a response has no tool calls, on Esc, or on a stream error.
//!
//! The screen is drawn from state every frame (`build_blocks`): the session's
//! messages, the turn in progress, and display-only notes. Handlers only change
//! state; drawing is the loop's job.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui_textarea::TextArea;
use serde_json::{Map, Value};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;

use crate::client::{self, StreamEvent, Verdict, VerdictKind};
use crate::commands::{self, Action, Completion};
use crate::config::{self, ApprovalMode, Settings};
use crate::input::{self, InputAction};
use crate::repl::{Repl, ReplEvent};
use crate::session::{FunctionCall, Message, NOT_RUN, Role, Session, ToolCall, reasoning_text};
use crate::tool_reasoning;
use crate::transcript::{Block, Key, View};
use crate::tui::{self, Tui};

const STOPPED: &str = "[stopped by user]";

/// Most rows the command completion list shows at once.
const COMPLETION_ROWS: usize = 8;

pub async fn run(settings: Settings, session: Session) -> anyhow::Result<()> {
    let repo_root = std::env::current_dir()?.display().to_string();
    let mut tui = Tui::enter()?;
    let mut app = App {
        settings,
        repo_root,
        session,
        http: reqwest::Client::new(),
        textarea: input::new_textarea(),
        view: View::default(),
        notes: Vec::new(),
        turn: None,
        notice: None,
        context_tokens: None,
        prompt_tokens: None,
        signaled: false,
        completion: 0,
        completion_dismissed: None,
    };
    let result = app.run(&mut tui).await;
    let exited = tui.exit();
    // After a signal the terminal may be gone, so restoring it can fail harmlessly.
    if !app.signaled {
        exited?;
    }
    result
}

struct App {
    settings: Settings,
    /// Absolute path of the working directory, given to the classifier.
    repo_root: String,
    session: Session,
    http: reqwest::Client,
    textarea: TextArea<'static>,
    view: View,
    /// Display-only lines (verdicts, errors), never saved.
    notes: Vec<Note>,
    turn: Option<Turn>,
    /// UI-only message shown in the status line until the next key press.
    notice: Option<String>,
    /// Context in use when the last reply finished (prompt + completion), as reported
    /// by the server. Unknown until a reply reports usage; stopped or failed replies
    /// report none, so the previous value stays.
    context_tokens: Option<u64>,
    /// Prompt tokens of the last reply, sent to the model as the next turn's FYI
    /// snapshot. Lags one turn behind; the first turn has no measurement yet.
    prompt_tokens: Option<u64>,
    /// Exiting because of a signal: the terminal may be gone, so a failure to
    /// restore it is ignored.
    signaled: bool,
    /// Highlighted row of the command completion list. Back to the first match
    /// whenever the input changes.
    completion: usize,
    /// Input text for which Esc closed the completion list; it stays closed until
    /// the input changes.
    completion_dismissed: Option<String>,
}

struct Turn {
    phase: Phase,
    /// Tool calls from the last response not yet handled, in order.
    queue: VecDeque<ToolCall>,
    /// Started by the turn's first call; dropping it kills the process.
    repl: Option<Repl>,
}

enum Phase {
    /// Between steps: `advance` handles the next queued call, or sends the next
    /// request once the queue is empty.
    Idle,
    Streaming(Reply),
    Classifying {
        call: PendingCall,
        task: JoinHandle<anyhow::Result<Verdict>>,
    },
    /// Waiting for y/n. `effects` is the classifier's description, if it gave one.
    Approving {
        call: PendingCall,
        effects: Option<String>,
    },
    /// Approved; `advance` runs it.
    Ready(PendingCall),
    Running {
        call: PendingCall,
        output: String,
    },
}

struct PendingCall {
    call: ToolCall,
    code: String,
}

/// A response in progress. Everything received so far.
struct Reply {
    task: JoinHandle<()>,
    events: UnboundedReceiver<StreamEvent>,
    /// The message assembled from the deltas so far (`client::accumulate`), every
    /// field as the endpoint sent it.
    fields: Map<String, Value>,
}

/// A line shown in the transcript but never sent to the model.
struct Note {
    anchor: Anchor,
    text: String,
    style: Style,
}

/// Where a note is shown. A note whose anchor is removed disappears with it.
enum Anchor {
    /// Under the call's code, above its result (classifier verdicts).
    Call(String),
    /// After the first `n` messages (errors). Adjusted when earlier
    /// messages are removed.
    After(usize),
}

enum Step {
    Signal,
    Input(Option<io::Result<Event>>),
    Turn(TurnEvent),
}

enum TurnEvent {
    Stream(StreamEvent),
    Verdict(anyhow::Result<Verdict>),
    Repl(anyhow::Result<ReplEvent>),
}

enum Flow {
    Continue,
    Exit,
}

impl App {
    async fn run(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let result = self.run_loop(tui).await;
        // Exiting mid-turn, including on an error or a signal, keeps the partial
        // reply and records results for its tool calls.
        let stopped = self.stop_turn(None);
        result.and(stopped)
    }

    async fn run_loop(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let mut input = spawn_input_reader();
        // Closing the terminal (SIGHUP) or a kill would otherwise skip cleanup,
        // orphaning the REPL and leaving tool calls without results. In raw mode
        // Ctrl+C is a key press, so SIGINT only arrives from outside.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;

        loop {
            let footer = self.footer();
            let blocks = build_blocks(
                self.settings.chat_prompt.as_deref(),
                &self.session.messages,
                &self.notes,
                self.turn.as_ref(),
                self.settings.tool_reasoning,
            );
            tui.draw(&blocks, &mut self.view, &self.textarea, &footer)?;
            // Biased toward input so keys (Esc above all) are handled before more
            // output from a fast stream.
            let step = tokio::select! {
                biased;
                _ = hangup.recv() => Step::Signal,
                _ = terminate.recv() => Step::Signal,
                _ = interrupt.recv() => Step::Signal,
                event = input.recv() => Step::Input(event),
                event = next_turn_event(&mut self.turn) => Step::Turn(event),
            };
            match step {
                Step::Signal => {
                    self.signaled = true;
                    return Ok(());
                }
                Step::Input(None) => return Ok(()),
                Step::Input(Some(event)) => {
                    if let Flow::Exit = self.on_input(tui, event?)? {
                        return Ok(());
                    }
                }
                Step::Turn(TurnEvent::Stream(event)) => self.on_stream(event)?,
                Step::Turn(TurnEvent::Verdict(verdict)) => self.on_verdict(verdict)?,
                Step::Turn(TurnEvent::Repl(event)) => self.on_repl(event)?,
            }
            self.advance().await?;
        }
    }

    /// Move the turn forward until it waits on something: the stream, the
    /// classifier, the user, or the REPL.
    async fn advance(&mut self) -> anyhow::Result<()> {
        loop {
            let Some(turn) = self.turn.as_mut() else {
                return Ok(());
            };
            match std::mem::replace(&mut turn.phase, Phase::Idle) {
                Phase::Idle => match turn.queue.pop_front() {
                    Some(call) => self.begin_call(call)?,
                    None => {
                        self.start_request();
                        return Ok(());
                    }
                },
                Phase::Ready(call) => self.run_call(call).await?,
                waiting => {
                    turn.phase = waiting;
                    return Ok(());
                }
            }
        }
    }

    fn start_request(&mut self) {
        let (task, events) = client::start(
            &self.http,
            &self.settings.endpoint,
            self.settings.chat_prompt.as_deref(),
            tool_reasoning::request_messages(&self.session.messages, self.settings.tool_reasoning),
        );
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = Phase::Streaming(Reply {
                task,
                events,
                fields: Map::new(),
            });
        }
    }

    fn on_stream(&mut self, event: StreamEvent) -> anyhow::Result<()> {
        let Some(Turn {
            phase: Phase::Streaming(reply),
            ..
        }) = &mut self.turn
        else {
            return Ok(());
        };
        // Take the events already queued too, so a fast stream costs one frame per
        // batch rather than one per delta.
        let mut event = Some(event);
        while let Some(current) = event.take() {
            match current {
                StreamEvent::Delta(delta) => client::accumulate(&mut reply.fields, delta),
                StreamEvent::Usage { total, prompt } => {
                    self.context_tokens = Some(total);
                    if let Some(prompt) = prompt {
                        self.prompt_tokens = Some(prompt);
                    }
                }
                StreamEvent::Done { truncated } => return self.end_stream(truncated),
                StreamEvent::Error(error) => return self.stop_turn(Some(error)),
            }
            event = reply.events.try_recv().ok();
        }
        Ok(())
    }

    /// The response finished: queue its tool calls, or end the turn. A response cut
    /// off at the token limit is treated as interrupted.
    fn end_stream(&mut self, truncated: bool) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Streaming(reply) = std::mem::replace(&mut turn.phase, Phase::Idle) else {
            return Ok(());
        };
        let (calls, saved) = self.save_reply(reply, truncated);
        // Queue the calls before anything can fail, so an error exit still records a
        // result for each of them.
        if calls.is_empty() {
            self.turn = None;
        } else if let Some(turn) = self.turn.as_mut() {
            turn.queue = calls.into();
        }
        saved
    }

    /// Save the response as an assistant message. Returns its complete tool calls and
    /// the save result (the message is in the session either way; a failed save is
    /// retried by the next one). Every field the endpoint sent is kept, so it gets
    /// its own format back. A call without an id or name is dropped, and when
    /// `interrupted`, so is a call whose arguments aren't valid JSON: it was still
    /// streaming and never ran, so nothing it did needs recording.
    fn save_reply(
        &mut self,
        reply: Reply,
        interrupted: bool,
    ) -> (Vec<ToolCall>, anyhow::Result<()>) {
        reply.task.abort();
        let mut fields = reply.fields;
        // The role is known, and `content` and `tool_calls` have typed homes; the rest
        // stays as streamed.
        fields.remove("role");
        let content = match fields.remove("content") {
            Some(Value::String(content)) => content,
            _ => String::new(),
        };
        let calls: Vec<ToolCall> = match fields.remove("tool_calls") {
            Some(Value::Array(calls)) => calls,
            _ => Vec::new(),
        }
        .into_iter()
        .filter_map(|mut call| {
            // `index` only orders fragments within the stream.
            call.as_object_mut()?.remove("index");
            let call: ToolCall = serde_json::from_value(call).ok()?;
            if interrupted && serde_json::from_str::<Value>(&call.function.arguments).is_err() {
                return None;
            }
            Some(call)
        })
        .collect();
        let message = Message {
            role: Role::Assistant,
            content,
            tool_calls: (!calls.is_empty()).then(|| calls.clone()),
            extra: fields,
            ..Default::default()
        };
        let saved = if message.is_empty() {
            Ok(())
        } else {
            self.session.push(message)
        };
        (calls, saved)
    }

    /// Start the call's approval according to the current mode. Calls that can't run
    /// get their result recorded here, leaving the turn `Idle`.
    ///
    /// Ordering rule (here and in every handler below): a call's result is recorded,
    /// or the call is placed back in turn state, before anything that can fail, so an
    /// error exit still leaves a result for every call.
    fn begin_call(&mut self, call: ToolCall) -> anyhow::Result<()> {
        let code = match code_of(&call) {
            Ok(code) => code,
            Err(error) => return self.record_result(&call, error),
        };
        let call = PendingCall { call, code };
        let phase = match self.settings.approval_mode {
            _ if is_exempt(&call.code) => Phase::Ready(call),
            // A no-op the model is meant to call freely. Kept out of `is_exempt`, which
            // also selects the calls the per-turn cleanup removes.
            _ if tool_reasoning::is_reasoning_call(&call.code) => Phase::Ready(call),
            ApprovalMode::Allow => Phase::Ready(call),
            ApprovalMode::Ask => Phase::Approving {
                call,
                effects: None,
            },
            ApprovalMode::Auto => {
                let http = self.http.clone();
                let endpoint = self.settings.endpoint.clone();
                let prompt = self.settings.classifier_prompt.clone();
                let repo_root = self.repo_root.clone();
                let code = call.code.clone();
                let task = tokio::spawn(async move {
                    client::classify(http, &endpoint, &prompt, &repo_root, &code).await
                });
                Phase::Classifying { call, task }
            }
        };
        self.set_phase(phase);
        Ok(())
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = phase;
        }
    }

    fn on_verdict(&mut self, verdict: anyhow::Result<Verdict>) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Classifying { call, .. } = std::mem::replace(&mut turn.phase, Phase::Idle)
        else {
            return Ok(());
        };
        let anchor = Anchor::Call(call.call.id.clone());
        match verdict {
            Ok(Verdict {
                effects,
                verdict: VerdictKind::Safe,
            }) => {
                self.set_phase(Phase::Ready(call));
                self.note(anchor, format!("safe: {effects}"), Style::new().green());
            }
            Ok(Verdict {
                effects,
                verdict: VerdictKind::Unsafe,
            }) => {
                self.record_result(&call.call, format!("Blocked: {effects}"))?;
                self.note(anchor, format!("unsafe: {effects}"), Style::new().red());
            }
            Ok(Verdict {
                effects,
                verdict: VerdictKind::Inconclusive,
            }) => {
                let line = format!("inconclusive: {effects}");
                self.set_phase(Phase::Approving {
                    call,
                    effects: Some(effects),
                });
                self.note(anchor, line, Style::new().yellow());
            }
            // A classifier failure asks the user rather than blocking or running.
            Err(error) => {
                self.set_phase(Phase::Approving {
                    call,
                    effects: None,
                });
                let line = format!("classifier failed: {error:#}");
                self.note(anchor, line, Style::new().yellow());
            }
        }
        Ok(())
    }

    fn note(&mut self, anchor: Anchor, text: String, style: Style) {
        self.notes.push(Note {
            anchor,
            text,
            style,
        });
    }

    /// Answer the approval prompt.
    fn approve(&mut self, allow: bool) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Approving { call, effects } = std::mem::replace(&mut turn.phase, Phase::Idle)
        else {
            return Ok(());
        };
        if allow {
            turn.phase = Phase::Ready(call);
            return Ok(());
        }
        let result = match effects {
            Some(effects) => format!("Denied by user: {effects}"),
            None => "Denied by user.".to_string(),
        };
        self.record_result(&call.call, result)
    }

    async fn run_call(&mut self, call: PendingCall) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        if turn.repl.is_none() {
            match Repl::start(self.settings.output_limit, &self.settings.endpoint.model) {
                Ok(repl) => turn.repl = Some(repl),
                Err(error) => {
                    let result = format!("REPL failed to start: {error:#}");
                    return self.record_result(&call.call, result);
                }
            }
        }
        let repl = turn.repl.as_mut().expect("started above");
        // The driver reads requests on a dedicated thread from startup, so this write
        // doesn't wait on library loading or on earlier code.
        if let Err(error) = repl.send(&call.code, self.prompt_tokens).await {
            // Dropping the REPL kills it; the next call starts a fresh one.
            turn.repl = None;
            return self.record_result(&call.call, format!("{error:#}"));
        }
        turn.phase = Phase::Running {
            call,
            output: String::new(),
        };
        Ok(())
    }

    fn on_repl(&mut self, event: anyhow::Result<ReplEvent>) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Running { output, .. } = &mut turn.phase else {
            return Ok(());
        };
        let (value, error) = match event {
            Ok(ReplEvent::Output(text)) => {
                output.push_str(&text);
                return Ok(());
            }
            Ok(ReplEvent::Done { value, error }) => (value, error),
            // The process died: its state is gone, and the next call starts a new one.
            Err(error) => {
                turn.repl = None;
                (None, Some(format!("[REPL process exited: {error:#}]")))
            }
        };
        let Phase::Running { call, mut output } = std::mem::replace(&mut turn.phase, Phase::Idle)
        else {
            return Ok(());
        };
        let extras: Vec<String> = [value, error].into_iter().flatten().collect();
        for extra in &extras {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(extra);
        }
        self.record_result(&call.call, output)
    }

    /// Record a call's result, capped at the output limit.
    fn record_result(&mut self, call: &ToolCall, result: String) -> anyhow::Result<()> {
        let result = if result.is_empty() {
            "(no output)".to_string()
        } else {
            truncate(result, self.settings.output_limit)
        };
        self.session.push(Message::tool(call.id.clone(), result))
    }

    /// End the turn now (Esc, `/exit`, a stream error, or app exit). The partial reply
    /// is kept; running code is killed and its output so far recorded; every other
    /// complete call gets a `[not run]` result so the session stays valid, shown under
    /// its call. `error` (a stream error) is shown as a note.
    fn stop_turn(&mut self, error: Option<String>) -> anyhow::Result<()> {
        let Some(mut turn) = self.turn.take() else {
            return Ok(());
        };
        // A failed save doesn't stop the rest: each message is already in the
        // session, and a later save writes it.
        let mut first_error: Option<anyhow::Error> = None;
        let mut check = |result: anyhow::Result<()>| {
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        };
        let mut not_run = Vec::new();
        match std::mem::replace(&mut turn.phase, Phase::Idle) {
            Phase::Idle => {}
            Phase::Streaming(reply) => {
                let (calls, saved) = self.save_reply(reply, true);
                check(saved);
                not_run = calls;
            }
            Phase::Classifying { call, task } => {
                task.abort();
                not_run.push(call.call);
            }
            Phase::Approving { call, .. } | Phase::Ready(call) => not_run.push(call.call),
            Phase::Running { call, mut output } => {
                if let Some(repl) = turn.repl.as_mut() {
                    repl.kill();
                }
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(STOPPED);
                check(self.record_result(&call.call, output));
            }
        }
        not_run.extend(turn.queue.drain(..));
        for call in &not_run {
            check(self.record_result(call, NOT_RUN.to_string()));
        }
        drop(turn); // Kills the REPL, if any.

        if let Some(error) = error {
            let end = Anchor::After(self.session.messages.len());
            self.note(end, format!("error: {error}"), Style::new().red());
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn on_input(&mut self, tui: &mut Tui, event: Event) -> anyhow::Result<Flow> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.notice = None;
                self.view.clear_selection();
                // Scroll keys work in every state, including at an approval prompt.
                let page = match key.code {
                    KeyCode::PageUp => Some(-1),
                    KeyCode::PageDown => Some(1),
                    _ => None,
                };
                if let Some(direction) = page {
                    self.view.page(direction);
                    return Ok(Flow::Continue);
                }
                // While a call awaits approval, only y/n, Esc, and Shift+Tab act.
                if matches!(
                    self.turn,
                    Some(Turn {
                        phase: Phase::Approving { .. },
                        ..
                    })
                ) {
                    match key.code {
                        KeyCode::Char('y') => self.approve(true)?,
                        KeyCode::Char('n') => self.approve(false)?,
                        KeyCode::Esc => self.stop_turn(None)?,
                        KeyCode::BackTab => self.cycle_mode(),
                        _ => {}
                    }
                    return Ok(Flow::Continue);
                }
                // While the completion list shows, it takes the keys that act on it.
                let completions = self.completions();
                if !completions.is_empty() && key.modifiers.is_empty() {
                    let count = completions.len();
                    let selected = self.completion.min(count - 1);
                    match key.code {
                        KeyCode::Up => {
                            self.completion = (selected + count - 1) % count;
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Down => {
                            self.completion = (selected + 1) % count;
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Tab => {
                            self.textarea.clear();
                            self.textarea.insert_str(completions[selected].spelling);
                            self.input_changed();
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Enter => {
                            return self.run_command(completions[selected].command.action);
                        }
                        KeyCode::Esc => {
                            self.completion_dismissed = Some(self.textarea.lines().join("\n"));
                            return Ok(Flow::Continue);
                        }
                        _ => {}
                    }
                }
                let before = self.textarea.lines().to_vec();
                let action = input::handle_key(&mut self.textarea, key);
                if self.textarea.lines() != before.as_slice() {
                    self.input_changed();
                }
                match action {
                    InputAction::Submit(text) => return self.submit(text),
                    InputAction::Stop => self.stop_turn(None)?,
                    InputAction::CycleMode => self.cycle_mode(),
                    InputAction::None => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.view.scroll(-1),
                MouseEventKind::ScrollDown => self.view.scroll(1),
                MouseEventKind::Down(MouseButton::Left) => self.view.press(mouse.column, mouse.row),
                MouseEventKind::Drag(MouseButton::Left) => self.view.drag(mouse.column, mouse.row),
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(text) = self.view.release()
                        && let Err(error) = tui.copy(&text)
                    {
                        self.notice = Some(format!("couldn't copy: {error}"));
                    }
                }
                _ => {}
            },
            Event::Paste(text) => {
                input::paste(&mut self.textarea, &text);
                self.input_changed();
            }
            // A resize needs nothing here: the next frame lays out at the new size.
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Shift+Tab: switch approval mode and persist it. Applies from the next call
    /// that hasn't been classified yet.
    fn cycle_mode(&mut self) {
        let mode = self.settings.approval_mode.next();
        self.settings.approval_mode = mode;
        if let Err(error) = config::save_approval_mode(mode) {
            self.notice = Some(format!("couldn't save approval_mode: {error:#}"));
        }
    }

    /// `/tool-reasoning`: switch tool reasoning and persist it. The transcript
    /// changes at once and the next request is built accordingly, since both are
    /// derived from the session.
    fn toggle_tool_reasoning(&mut self) {
        let enabled = !self.settings.tool_reasoning;
        self.settings.tool_reasoning = enabled;
        if let Err(error) = config::save_tool_reasoning(enabled) {
            self.notice = Some(format!("couldn't save tool_reasoning: {error:#}"));
        }
    }

    /// Run a command (typed in full, or picked from the completion list). The input
    /// is cleared.
    fn run_command(&mut self, action: Action) -> anyhow::Result<Flow> {
        self.textarea.clear();
        self.input_changed();
        match action {
            Action::Exit => return Ok(Flow::Exit),
            Action::ToggleToolReasoning => self.toggle_tool_reasoning(),
        }
        Ok(Flow::Continue)
    }

    /// Completions for the input, or none when the list isn't showing: the input must
    /// be one line starting with `/` and without whitespace, Esc must not have closed
    /// the list for it, and no approval prompt may be showing (the list replaces the
    /// status line, which carries the prompt).
    fn completions(&self) -> Vec<Completion> {
        let approving = matches!(
            self.turn,
            Some(Turn {
                phase: Phase::Approving { .. },
                ..
            })
        );
        let [input] = self.textarea.lines() else {
            return Vec::new();
        };
        if approving
            || !input.starts_with('/')
            || input.contains(char::is_whitespace)
            || self.completion_dismissed.as_ref() == Some(input)
        {
            return Vec::new();
        }
        commands::completions(input)
    }

    /// The input text changed: highlight the first completion again and reopen a
    /// list closed with Esc.
    fn input_changed(&mut self) {
        self.completion = 0;
        self.completion_dismissed = None;
    }

    /// Rows below the input: the completion list while it shows, otherwise the
    /// status line.
    fn footer(&self) -> Vec<Line<'static>> {
        let completions = self.completions();
        if completions.is_empty() {
            return vec![self.status_line()];
        }
        let selected = self.completion.min(completions.len() - 1);
        // A window of rows that keeps the highlighted one in view.
        let first = (selected + 1).saturating_sub(COMPLETION_ROWS);
        let width = completions
            .iter()
            .map(|completion| completion.spelling.len())
            .max()
            .unwrap_or(0);
        completions
            .iter()
            .enumerate()
            .skip(first)
            .take(COMPLETION_ROWS)
            .map(|(index, completion)| {
                // Indented to line up with the input text after the `> ` prompt.
                let name = format!("  {:width$}   ", completion.spelling);
                let description = completion.command.description;
                if index == selected {
                    let reversed = Style::default().reversed();
                    Line::from(vec![
                        Span::styled(name, reversed),
                        Span::styled(description, reversed),
                    ])
                } else {
                    Line::from(vec![Span::raw(name), Span::styled(description, tui::dim())])
                }
            })
            .collect()
    }

    /// Commands run in any state and keep an unknown command in the input for
    /// correction. A message is sent only when no turn is in progress; otherwise the
    /// draft is kept.
    fn submit(&mut self, text: String) -> anyhow::Result<Flow> {
        if text.starts_with('/') {
            let command = text.trim_end();
            return match commands::find(command) {
                Some(action) => self.run_command(action),
                None => {
                    self.notice = Some(format!("unknown command: {command}"));
                    Ok(Flow::Continue)
                }
            };
        }
        if self.turn.is_some() {
            self.notice = Some("turn in progress (esc to stop)".to_string());
            return Ok(Flow::Continue);
        }

        self.textarea.clear();
        self.view.scroll_to_bottom();
        // Only the current turn's synthetic calls stay in context: remove every
        // earlier call made only of `FYI()`/`help()` (synthetic or the model's own)
        // before adding this turn's. The session file is the prompt, so they leave
        // the file too.
        let removed = self
            .session
            .remove_calls(|call| code_of(call).is_ok_and(|code| is_exempt(&code)))?;
        // Notes placed after a message keep their place among the messages left.
        for note in &mut self.notes {
            if let Anchor::After(n) = &mut note.anchor {
                *n -= removed.partition_point(|&index| index < *n);
            }
        }
        self.session.push(Message::user(text))?;

        // Each turn opens with two synthetic calls that the app makes as if the model
        // had: `FYI()` and `help()`, both exempt from approval. `advance` runs them,
        // then sends the first request.
        let calls = fyi_calls();
        let saved = self.session.push(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(calls.clone()),
            ..Default::default()
        });
        // Queue the calls before the save result is checked: the message is in the
        // session either way, and an error exit must still record their results.
        self.turn = Some(Turn {
            phase: Phase::Idle,
            queue: calls.into(),
            repl: None,
        });
        saved?;
        Ok(Flow::Continue)
    }

    fn status_line(&self) -> Line<'static> {
        let mut info = self.settings.endpoint.model.clone();
        if let Some(tokens) = self.context_tokens {
            info.push_str(&format!(" | {} tokens", thousands(tokens)));
        }
        info.push_str(&format!(" | {}", self.settings.approval_mode.as_str()));
        if self.settings.tool_reasoning {
            info.push_str(" | tool reasoning");
        }
        let mut spans = vec![Span::styled(info, tui::dim())];
        let activity = match self.turn.as_ref().map(|t| &t.phase) {
            Some(Phase::Streaming(_)) => Some("responding… (esc to stop)"),
            Some(Phase::Classifying { .. }) => Some("classifying… (esc to stop)"),
            Some(Phase::Running { .. }) => Some("running… (esc to stop)"),
            _ => None,
        };
        if let Some(activity) = activity {
            spans.push(Span::styled(format!(" · {activity}"), tui::dim()));
        }
        if let Some(Phase::Approving { .. }) = self.turn.as_ref().map(|t| &t.phase) {
            spans.push(Span::styled(" · ", tui::dim()));
            spans.push(Span::styled(
                "allow? (y/n, esc to stop)",
                Style::default().yellow(),
            ));
        }
        if self.view.scrolled_up() {
            spans.push(Span::styled(" · scrolled up (PgDn)", tui::dim()));
        }
        if let Some(notice) = &self.notice {
            spans.push(Span::styled(" · ", tui::dim()));
            spans.push(Span::styled(notice.clone(), Style::default().yellow()));
        }
        Line::from(spans)
    }
}

/// The transcript as drawn, so it always matches what the model sees: the system
/// message, the messages (each tool result under its call, matched by id, rather
/// than in file order), and the turn in progress. Notes appear where anchored.
///
/// Each block gets a `Key` naming what it shows, so the view's scroll position and
/// selection stay on the same text when blocks are inserted before it. A call's
/// result reuses the key of its live output, which it replaces.
fn build_blocks<'a>(
    system: Option<&'a str>,
    messages: &'a [Message],
    notes: &'a [Note],
    turn: Option<&'a Turn>,
    tool_reasoning_enabled: bool,
) -> Vec<Block<'a>> {
    let blank = |key| Block::new(key, "", plain());
    let turn_start = tool_reasoning::turn_start(messages);
    let mut blocks = Vec::new();
    if let Some(system) = system {
        blocks.push(Block::new(Key::System(0), "system", tui::dim()));
        blocks.push(Block::new(Key::System(1), system, plain()));
        blocks.push(blank(Key::System(2)));
    }

    let results: HashMap<&str, &str> = messages
        .iter()
        .filter_map(|m| Some((m.tool_call_id.as_deref()?, m.content.as_str())))
        .collect();
    let mut call_notes: HashMap<&str, Vec<&Note>> = HashMap::new();
    let mut position_notes = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        match &note.anchor {
            Anchor::Call(id) => call_notes.entry(id).or_default().push(note),
            Anchor::After(n) => position_notes.push((*n, index, note)),
        }
    }
    // Notes are added in order, and cleanup keeps their positions in order.
    let mut position_notes = position_notes.into_iter().peekable();
    let mut add_position_notes = |through: usize, blocks: &mut Vec<Block<'a>>| {
        while let Some((_, index, note)) = position_notes.next_if(|&(n, ..)| n <= through) {
            blocks.push(Block::new(
                Key::Note(index, 0),
                note.text.as_str(),
                note.style,
            ));
            blocks.push(blank(Key::Note(index, 1)));
        }
    };
    let (phase, running) = match turn.map(|t| &t.phase) {
        Some(Phase::Running { call, output }) => (None, Some((&call.call.id, output))),
        phase => (phase, None),
    };

    for (i, message) in messages.iter().enumerate() {
        add_position_notes(i, &mut blocks);
        let part = |n| Key::Message(i, n);
        match message.role {
            Role::User => {
                let text = format!("> {}", message.content);
                blocks.push(Block::new(part(0), text, user_style()));
                blocks.push(blank(part(1)));
            }
            Role::Assistant => {
                // Converted reasoning is drawn as the synthetic call it's sent as, ahead
                // of the message. Drawn inline rather than as extra messages, so message
                // indices (keys, note anchors) match the session.
                let converted = tool_reasoning_enabled
                    .then(|| tool_reasoning::converted(messages, i, turn_start))
                    .flatten();
                if let Some(text) = &converted {
                    let part = |n| Key::Call(tool_reasoning::call_id(i), n);
                    blocks.push(Block::new(part(0), "REPL", tui::dim()));
                    blocks.push(Block::new(part(1), tool_reasoning::code(text), plain()));
                    let result = tool_reasoning::RESULT;
                    blocks.push(Block::new(part(2), result, tool_output_style()));
                    blocks.push(blank(part(3)));
                }
                // Only reasoning, content, and calls are drawn; other fields are sent
                // back but have no display form.
                let reasoning = message.reasoning().filter(|_| converted.is_none());
                let has_reasoning = reasoning.is_some();
                if let Some(reasoning) = reasoning {
                    blocks.push(Block::new(part(0), reasoning, reasoning_style()));
                    if !message.content.is_empty() {
                        blocks.push(blank(part(1)));
                    }
                }
                if !message.content.is_empty() {
                    blocks.push(Block::new(part(2), message.content.as_str(), plain()));
                }
                if has_reasoning || !message.content.is_empty() {
                    blocks.push(blank(part(3)));
                }
                for call in message.tool_calls.iter().flatten() {
                    // Parts: 0 label, 1 code, 2 result or live output, 3 blank, 4+ notes.
                    let part = |n| Key::Call(call.id.clone(), n);
                    match code_of(call) {
                        Ok(code) => {
                            blocks.push(Block::new(part(0), "REPL", tui::dim()));
                            blocks.push(Block::new(part(1), code, plain()));
                        }
                        Err(_) => {
                            let arguments = call.function.arguments.as_str();
                            blocks.push(Block::new(part(0), arguments, plain()))
                        }
                    }
                    let call_notes = call_notes.get(call.id.as_str()).into_iter().flatten();
                    for (n, note) in call_notes.enumerate() {
                        let key = part(4 + n as u8);
                        blocks.push(Block::new(key, note.text.as_str(), note.style));
                    }
                    if let Some(result) = results.get(call.id.as_str()) {
                        blocks.push(Block::new(part(2), *result, tool_output_style()));
                    } else if let Some((id, output)) = running
                        && *id == call.id
                        && !output.is_empty()
                    {
                        blocks.push(Block::new(part(2), output.as_str(), tool_output_style()));
                    }
                    blocks.push(blank(part(3)));
                }
            }
            Role::Tool => {}
        }
    }
    add_position_notes(usize::MAX, &mut blocks);

    // Keyed as the message it will be saved as, so saving it changes no keys.
    if let Some(Phase::Streaming(reply)) = phase {
        // Tool calls aren't drawn while streaming; they appear once the reply is saved.
        let part = |n| Key::Message(messages.len(), n);
        let reasoning = reasoning_text(&reply.fields);
        let has_reasoning = reasoning.is_some();
        if let Some(reasoning) = reasoning {
            blocks.push(Block::new(part(0), reasoning, reasoning_style()));
        }
        let content = reply.fields.get("content").and_then(Value::as_str);
        if let Some(content) = content.filter(|content| !content.is_empty()) {
            if has_reasoning {
                blocks.push(blank(part(1)));
            }
            blocks.push(Block::new(part(2), content, plain()));
        }
    }
    blocks
}

/// The two synthetic calls opening each turn: `FYI()` for the environment
/// snapshot and `help()` for the library listing. Ids only need to be unique within
/// a session: the timestamp keeps them unique across `--resume`, the counter
/// within one millisecond.
fn fyi_calls() -> Vec<ToolCall> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    ["FYI()", "help()"]
        .into_iter()
        .map(|code| ToolCall {
            id: format!(
                "fyi-{}-{}",
                chrono::Local::now().timestamp_millis(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "REPL".to_string(),
                arguments: format!(r#"{{"code": "{code}"}}"#),
                extra: Map::new(),
            },
            extra: Map::new(),
        })
        .collect()
}

/// Calls that never need approval in any mode: code made only of the driver's
/// read-only built-ins `FYI()` and `help()`, as statements separated by `;` or
/// newlines. Any other code alongside them (`FYI(); os.remove(p)`) is judged
/// normally, and so is `help(x)`, since its argument is evaluated.
fn is_exempt(code: &str) -> bool {
    let mut statements = code
        .split([';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .peekable();
    statements.peek().is_some() && statements.all(|s| matches!(s, "FYI()" | "help()"))
}

/// The `code` argument of a REPL call, or the error to return to the model.
fn code_of(call: &ToolCall) -> Result<String, String> {
    if call.function.name != "REPL" {
        return Err(format!("Unknown tool: {}", call.function.name));
    }
    let arguments: Value = serde_json::from_str(&call.function.arguments)
        .map_err(|e| format!("Invalid arguments ({e}): {}", call.function.arguments))?;
    arguments["code"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "Invalid arguments: missing string `code`".to_string())
}

/// Cap `text` at `limit` bytes (on a character boundary), noting how much was cut.
fn truncate(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated: {} more bytes]",
        &text[..end],
        text.len() - end
    )
}

/// `32093` → `32,093`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plain() -> Style {
    Style::new()
}

fn user_style() -> Style {
    Style::new().cyan()
}

fn reasoning_style() -> Style {
    tui::dim()
}

fn tool_output_style() -> Style {
    tui::dim()
}

async fn next_turn_event(turn: &mut Option<Turn>) -> TurnEvent {
    let Some(turn) = turn else {
        return std::future::pending().await;
    };
    match &mut turn.phase {
        Phase::Streaming(reply) => {
            TurnEvent::Stream(reply.events.recv().await.unwrap_or_else(|| {
                StreamEvent::Error("request task ended without a result".to_string())
            }))
        }
        Phase::Classifying { task, .. } => TurnEvent::Verdict(match task.await {
            Ok(verdict) => verdict,
            Err(error) => Err(error.into()),
        }),
        Phase::Running { .. } => match turn.repl.as_mut() {
            Some(repl) => TurnEvent::Repl(repl.next_event().await),
            None => std::future::pending().await,
        },
        Phase::Idle | Phase::Approving { .. } | Phase::Ready(_) => std::future::pending().await,
    }
}

/// Read terminal events on a dedicated thread. It polls with a timeout instead of
/// blocking in `read` so it notices when the app has stopped listening and exits.
fn spawn_input_reader() -> UnboundedReceiver<io::Result<Event>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(20)) {
                Ok(true) => {
                    if tx.send(event::read()).is_err() {
                        return;
                    }
                }
                Ok(false) => {
                    if tx.is_closed() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    rx
}
