//! Chat loop: input events, the tool-calling turn, and session persistence.
//!
//! A turn starts with a user message and alternates between streaming a response and
//! handling its tool calls (classify or ask, then run in the turn's REPL). It ends
//! when a response has no tool calls, on Esc, or on a stream error.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::{ContentStyle, Stylize};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui_textarea::TextArea;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;

use crate::client::{self, StreamEvent, Verdict, VerdictKind};
use crate::config::{self, ApprovalMode, Settings};
use crate::input::{self, InputAction};
use crate::repl::{Repl, ReplEvent};
use crate::session::{FunctionCall, Message, NOT_RUN, Role, Session, ToolCall};
use crate::tui::{self, Tui};

const STOPPED: &str = "[stopped by user]";

pub async fn run(settings: Settings, session: Session) -> anyhow::Result<()> {
    let repo_root = std::env::current_dir()?.display().to_string();
    let mut tui = Tui::enter()?;
    let mut app = App {
        settings,
        repo_root,
        session,
        http: reqwest::Client::new(),
        textarea: input::new_textarea(),
        turn: None,
        notice: None,
        context_tokens: None,
        signaled: false,
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
    turn: Option<Turn>,
    /// UI-only message shown in the status line until the next key press.
    notice: Option<String>,
    /// Context in use when the last reply finished (prompt + completion), as reported
    /// by the server. Unknown until a reply reports usage; stopped or failed replies
    /// report none, so the previous value stays.
    context_tokens: Option<u64>,
    /// Exiting because of a signal: skip terminal output during cleanup.
    signaled: bool,
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
    reasoning: String,
    content: String,
    calls: Vec<PartialCall>,
    /// Content has started displaying (after the blank line separating it from any
    /// reasoning shown above it).
    content_shown: bool,
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
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
        let tui = (!self.signaled).then_some(tui);
        let stopped = self.stop_turn(tui, None);
        result.and(stopped)
    }

    async fn run_loop(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        print_transcript(tui, &self.session.messages)?;
        let mut input = spawn_input_reader();
        // Closing the terminal (SIGHUP) or a kill would otherwise skip cleanup,
        // orphaning the REPL and leaving tool calls without results. In raw mode
        // Ctrl+C is a key press, so SIGINT only arrives from outside.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;

        loop {
            tui.draw(&self.textarea, self.status_line())?;
            // Biased toward input so a queued resize is handled before more transcript
            // output is placed using pre-resize coordinates.
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
                Step::Turn(TurnEvent::Stream(event)) => self.on_stream(tui, event)?,
                Step::Turn(TurnEvent::Verdict(verdict)) => self.on_verdict(tui, verdict)?,
                Step::Turn(TurnEvent::Repl(event)) => self.on_repl(tui, event)?,
            }
            self.advance(tui).await?;
        }
    }

    /// Move the turn forward until it waits on something: the stream, the
    /// classifier, the user, or the REPL.
    async fn advance(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        loop {
            let Some(turn) = self.turn.as_mut() else {
                return Ok(());
            };
            match std::mem::replace(&mut turn.phase, Phase::Idle) {
                Phase::Idle => match turn.queue.pop_front() {
                    Some(call) => self.begin_call(tui, call)?,
                    None => {
                        self.start_request();
                        return Ok(());
                    }
                },
                Phase::Ready(call) => self.run_call(tui, call).await?,
                waiting => {
                    turn.phase = waiting;
                    return Ok(());
                }
            }
        }
    }

    fn start_request(&mut self) {
        let (task, events) =
            client::start(&self.http, &self.settings.endpoint, &self.session.messages);
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = Phase::Streaming(Reply {
                task,
                events,
                reasoning: String::new(),
                content: String::new(),
                calls: Vec::new(),
                content_shown: false,
            });
        }
    }

    fn on_stream(&mut self, tui: &mut Tui, event: StreamEvent) -> anyhow::Result<()> {
        let first = match event {
            StreamEvent::Done { truncated } => return self.end_stream(tui, truncated),
            StreamEvent::Error(error) => return self.stop_turn(Some(tui), Some(error)),
            StreamEvent::Usage(total) => {
                self.context_tokens = Some(total);
                return Ok(());
            }
            delta => delta,
        };
        let Some(Turn {
            phase: Phase::Streaming(reply),
            ..
        }) = &mut self.turn
        else {
            return Ok(());
        };
        // Coalesce deltas that are already queued: each append waits on a terminal
        // round-trip, so appending them one at a time would fall behind a fast stream.
        // Consecutive deltas of the same kind merge into one run.
        let mut runs: Vec<(bool, String)> = Vec::new(); // (is_reasoning, text)
        let mut next = None;
        let mut event = Some(first);
        while let Some(current) = event.take() {
            let (is_reasoning, text) = match current {
                StreamEvent::Reasoning(text) => (true, text),
                StreamEvent::Content(text) => (false, text),
                StreamEvent::ToolCall {
                    index,
                    id,
                    name,
                    arguments,
                } => {
                    // Tool calls aren't displayed while streaming; the code is shown
                    // once the call is handled.
                    if reply.calls.len() <= index {
                        reply.calls.resize_with(index + 1, PartialCall::default);
                    }
                    let call = &mut reply.calls[index];
                    call.id = call.id.take().or(id);
                    call.name = call.name.take().or(name);
                    call.arguments.push_str(&arguments);
                    event = reply.events.try_recv().ok();
                    continue;
                }
                StreamEvent::Usage(total) => {
                    self.context_tokens = Some(total);
                    event = reply.events.try_recv().ok();
                    continue;
                }
                end => {
                    next = Some(end);
                    break;
                }
            };
            // Record before terminal I/O so a terminal error can't lose received text.
            if is_reasoning {
                reply.reasoning.push_str(&text);
            } else {
                reply.content.push_str(&text);
            }
            match runs.last_mut() {
                Some((kind, run)) if *kind == is_reasoning => run.push_str(&text),
                _ => runs.push((is_reasoning, text)),
            }
            event = reply.events.try_recv().ok();
        }

        for (is_reasoning, text) in runs {
            if is_reasoning {
                tui.append(&text, reasoning_style())?;
                continue;
            }
            if !reply.content_shown {
                reply.content_shown = true;
                if !reply.reasoning.is_empty() {
                    tui.print("", plain())?;
                }
            }
            tui.append(&text, plain())?;
        }
        match next {
            Some(event) => self.on_stream(tui, event),
            None => Ok(()),
        }
    }

    /// The response finished: queue its tool calls, or end the turn. A response cut
    /// off at the token limit is treated as interrupted.
    fn end_stream(&mut self, tui: &mut Tui, truncated: bool) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Streaming(reply) = std::mem::replace(&mut turn.phase, Phase::Idle) else {
            return Ok(());
        };
        let (calls, shown, saved) = self.save_reply(reply, truncated);
        // Queue the calls before anything can fail, so an error exit still records a
        // result for each of them.
        if calls.is_empty() {
            self.turn = None;
        } else if let Some(turn) = self.turn.as_mut() {
            turn.queue = calls.into();
        }
        saved?;
        show_reply_end(tui, shown, None)
    }

    /// Save the response as an assistant message. Returns its complete tool calls,
    /// whether any text was shown, and the save result (the message is in the session
    /// either way; a failed save is retried by the next one). When `interrupted`, a
    /// call whose arguments aren't valid JSON was still streaming and is dropped; it
    /// never ran, so nothing it did needs recording. Does no terminal I/O.
    fn save_reply(
        &mut self,
        reply: Reply,
        interrupted: bool,
    ) -> (Vec<ToolCall>, bool, anyhow::Result<()>) {
        reply.task.abort();
        let calls: Vec<ToolCall> = reply
            .calls
            .into_iter()
            .filter_map(|p| {
                let (id, name) = (p.id?, p.name?);
                if interrupted && serde_json::from_str::<Value>(&p.arguments).is_err() {
                    return None;
                }
                Some(ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: p.arguments,
                    },
                })
            })
            .collect();
        let shown = !reply.reasoning.is_empty() || !reply.content.is_empty();
        let saved = if shown || !calls.is_empty() {
            self.session.push(Message {
                role: Role::Assistant,
                content: reply.content,
                reasoning: (!reply.reasoning.is_empty()).then_some(reply.reasoning),
                tool_calls: (!calls.is_empty()).then(|| calls.clone()),
                ..Default::default()
            })
        } else {
            Ok(())
        };
        (calls, shown, saved)
    }

    /// Start the call's approval according to the current mode, then show it. Calls
    /// that can't run get their result recorded here, leaving the turn `Idle`.
    ///
    /// Ordering rule (here and in every handler below): a call's result is recorded,
    /// or the call is placed back in turn state, before any terminal I/O. Terminal
    /// calls can fail, and an error exit must still leave a result for every call.
    fn begin_call(&mut self, tui: &mut Tui, call: ToolCall) -> anyhow::Result<()> {
        let code = match code_of(&call) {
            Ok(code) => code,
            Err(error) => {
                self.record_result(&call, error.clone())?;
                tui.print(&call.function.arguments, plain())?;
                return show_result(tui, &error);
            }
        };
        let call = PendingCall {
            call,
            code: code.clone(),
        };
        let phase = match self.settings.approval_mode {
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
        Ok(show_code(tui, &code)?)
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = phase;
        }
    }

    fn on_verdict(
        &mut self,
        tui: &mut Tui,
        verdict: anyhow::Result<Verdict>,
    ) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Classifying { call, .. } = std::mem::replace(&mut turn.phase, Phase::Idle)
        else {
            return Ok(());
        };
        match verdict {
            Ok(Verdict {
                effects,
                verdict: VerdictKind::Safe,
            }) => {
                self.set_phase(Phase::Ready(call));
                Ok(tui.print(&format!("safe: {effects}"), plain().green())?)
            }
            Ok(Verdict {
                effects,
                verdict: VerdictKind::Unsafe,
            }) => {
                let result = format!("Blocked: {effects}");
                self.record_result(&call.call, result.clone())?;
                tui.print(&format!("unsafe: {effects}"), plain().red())?;
                show_result(tui, &result)
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
                Ok(tui.print(&line, plain().yellow())?)
            }
            // A classifier failure asks the user rather than blocking or running.
            Err(error) => {
                self.set_phase(Phase::Approving {
                    call,
                    effects: None,
                });
                Ok(tui.print(&format!("classifier failed: {error:#}"), plain().yellow())?)
            }
        }
    }

    /// Answer the approval prompt.
    fn approve(&mut self, tui: &mut Tui, allow: bool) -> anyhow::Result<()> {
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
        self.record_result(&call.call, result.clone())?;
        show_result(tui, &result)
    }

    async fn run_call(&mut self, tui: &mut Tui, call: PendingCall) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        if turn.repl.is_none() {
            match Repl::start(self.settings.output_limit) {
                Ok(repl) => turn.repl = Some(repl),
                Err(error) => {
                    let result = format!("REPL failed to start: {error:#}");
                    self.record_result(&call.call, result.clone())?;
                    return show_result(tui, &result);
                }
            }
        }
        let repl = turn.repl.as_mut().expect("started above");
        // The driver reads requests on a dedicated thread from startup, so this write
        // doesn't wait on library loading or on earlier code.
        if let Err(error) = repl.send(&call.code).await {
            // Dropping the REPL kills it; the next call starts a fresh one.
            turn.repl = None;
            let result = format!("{error:#}");
            self.record_result(&call.call, result.clone())?;
            return show_result(tui, &result);
        }
        turn.phase = Phase::Running {
            call,
            output: String::new(),
        };
        Ok(())
    }

    fn on_repl(&mut self, tui: &mut Tui, event: anyhow::Result<ReplEvent>) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Running { output, .. } = &mut turn.phase else {
            return Ok(());
        };
        let (value, error) = match event {
            Ok(ReplEvent::Output(text)) => {
                // Record before terminal I/O, as with streamed replies.
                output.push_str(&text);
                return Ok(tui.append(&text, tool_output_style())?);
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
        let empty = output.is_empty();
        self.record_result(&call.call, output)?;

        tui.end_line()?;
        for extra in &extras {
            tui.print(extra.trim_end_matches('\n'), tool_output_style())?;
        }
        if empty {
            tui.print("(no output)", tool_output_style())?;
        }
        Ok(tui.print("", plain())?)
    }

    /// Record a call's result, capped at the output limit. No terminal I/O.
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
    /// complete call gets a `[not run]` result so the session stays valid.
    ///
    /// `tui` is `None` when exiting on a signal: the terminal may be gone, and each
    /// print would wait out the cursor-position timeout, so only recording happens.
    fn stop_turn(&mut self, tui: Option<&mut Tui>, error: Option<String>) -> anyhow::Result<()> {
        let Some(mut turn) = self.turn.take() else {
            return Ok(());
        };
        // Record everything before any terminal I/O. A failed save doesn't stop the
        // rest: each message is already in the session, and a later save writes it.
        let mut first_error: Option<anyhow::Error> = None;
        let mut note = |result: anyhow::Result<()>| {
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        };
        let mut not_run = Vec::new();
        let mut reply_shown = None;
        let mut stopped = false;
        match std::mem::replace(&mut turn.phase, Phase::Idle) {
            Phase::Idle => {}
            Phase::Streaming(reply) => {
                let (calls, shown, saved) = self.save_reply(reply, true);
                note(saved);
                not_run = calls;
                reply_shown = Some(shown);
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
                note(self.record_result(&call.call, output));
                stopped = true;
            }
        }
        not_run.extend(turn.queue.drain(..));
        for call in &not_run {
            note(self.record_result(call, NOT_RUN.to_string()));
        }
        drop(turn); // Kills the REPL, if any.
        if let Some(e) = first_error {
            return Err(e);
        }

        let Some(tui) = tui else {
            return Ok(());
        };
        if let Some(shown) = reply_shown {
            show_reply_end(tui, shown, error)?;
        }
        if stopped {
            tui.end_line()?;
            show_result(tui, STOPPED)?;
        }
        if !not_run.is_empty() {
            show_result(tui, &format!("{NOT_RUN} ({} tool calls)", not_run.len()))?;
        }
        Ok(())
    }

    fn on_input(&mut self, tui: &mut Tui, event: Event) -> anyhow::Result<Flow> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.notice = None;
                // While a call awaits approval, only y/n, Esc, and Shift+Tab act.
                if matches!(
                    self.turn,
                    Some(Turn {
                        phase: Phase::Approving { .. },
                        ..
                    })
                ) {
                    match key.code {
                        KeyCode::Char('y') => self.approve(tui, true)?,
                        KeyCode::Char('n') => self.approve(tui, false)?,
                        KeyCode::Esc => self.stop_turn(Some(tui), None)?,
                        KeyCode::BackTab => self.cycle_mode(),
                        _ => {}
                    }
                    return Ok(Flow::Continue);
                }
                match input::handle_key(&mut self.textarea, key) {
                    InputAction::Submit(text) => return self.submit(tui, text),
                    InputAction::Stop => self.stop_turn(Some(tui), None)?,
                    InputAction::CycleMode => self.cycle_mode(),
                    InputAction::None => {}
                }
            }
            Event::Paste(text) => input::paste(&mut self.textarea, &text),
            Event::Resize(..) => tui.handle_resize()?,
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

    /// Commands run in any state and keep an unknown command in the input for
    /// correction. A message is sent only when no turn is in progress; otherwise the
    /// draft is kept.
    fn submit(&mut self, tui: &mut Tui, text: String) -> anyhow::Result<Flow> {
        if text.starts_with('/') {
            match text.trim_end() {
                "/exit" | "/quit" => return Ok(Flow::Exit),
                command => self.notice = Some(format!("unknown command: {command}")),
            }
            return Ok(Flow::Continue);
        }
        if self.turn.is_some() {
            self.notice = Some("turn in progress (esc to stop)".to_string());
            return Ok(Flow::Continue);
        }

        self.textarea.clear();
        // Save before terminal I/O so a terminal error can't lose the message.
        self.session.push(Message::user(text))?;
        print_transcript(
            tui,
            std::slice::from_ref(self.session.messages.last().expect("just pushed")),
        )?;
        // `advance` sends the first request.
        self.turn = Some(Turn {
            phase: Phase::Idle,
            queue: VecDeque::new(),
            repl: None,
        });
        Ok(Flow::Continue)
    }

    fn status_line(&self) -> Line<'static> {
        let mut info = self.settings.endpoint.model.clone();
        if let Some(tokens) = self.context_tokens {
            info.push_str(&format!(" | {} tokens", thousands(tokens)));
        }
        info.push_str(&format!(" | {}", self.settings.approval_mode.as_str()));
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
        if let Some(notice) = &self.notice {
            spans.push(Span::styled(" · ", tui::dim()));
            spans.push(Span::styled(notice.clone(), Style::default().yellow()));
        }
        Line::from(spans)
    }
}

/// Print messages as they looked live. Tool results are printed under their calls,
/// matched by id, rather than in file order.
fn print_transcript(tui: &mut Tui, messages: &[Message]) -> io::Result<()> {
    for message in messages {
        match message.role {
            Role::User => {
                tui.print(&format!("> {}", message.content), plain().cyan())?;
                tui.print("", plain())?;
            }
            Role::Assistant => {
                if let Some(reasoning) = &message.reasoning {
                    tui.print(reasoning, reasoning_style())?;
                    if !message.content.is_empty() {
                        tui.print("", plain())?;
                    }
                }
                if !message.content.is_empty() {
                    tui.print(&message.content, plain())?;
                }
                if message.reasoning.is_some() || !message.content.is_empty() {
                    tui.print("", plain())?;
                }
                for call in message.tool_calls.iter().flatten() {
                    match code_of(call) {
                        Ok(code) => show_code(tui, &code)?,
                        Err(_) => tui.print(&call.function.arguments, plain())?,
                    }
                    let result = messages
                        .iter()
                        .find(|m| m.tool_call_id.as_deref() == Some(&call.id));
                    if let Some(result) = result {
                        tui.print(&result.content, tool_output_style())?;
                    }
                    tui.print("", plain())?;
                }
            }
            Role::Tool => {}
        }
    }
    Ok(())
}

/// A tool result that wasn't streamed live, closing the call's block.
fn show_result(tui: &mut Tui, text: &str) -> anyhow::Result<()> {
    tui.print(text, tool_output_style())?;
    Ok(tui.print("", plain())?)
}

/// Close a response's display: end its last line, then the blank line after any
/// text, then the error, if there was one.
fn show_reply_end(tui: &mut Tui, shown: bool, error: Option<String>) -> anyhow::Result<()> {
    tui.end_line()?;
    if shown {
        tui.print("", plain())?;
    }
    if let Some(error) = error {
        tui.print(&format!("error: {error}"), plain().red())?;
        tui.print("", plain())?;
    }
    Ok(())
}

fn show_code(tui: &mut Tui, code: &str) -> io::Result<()> {
    tui.print("REPL", plain().dim())?;
    tui.print(code.trim_end_matches('\n'), plain())
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

fn plain() -> ContentStyle {
    ContentStyle::new()
}

fn reasoning_style() -> ContentStyle {
    ContentStyle::new().dim()
}

fn tool_output_style() -> ContentStyle {
    ContentStyle::new().dim()
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
/// blocking in `read` (or using crossterm's `EventStream`) because crossterm holds its
/// input lock while waiting, and `cursor::position()`, which the terminal layer uses
/// after every print, needs that lock. A `position()` call can wait out the rest of
/// the current poll window, so the window is kept short.
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
