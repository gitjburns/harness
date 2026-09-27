# A no-op. The app never runs this body for the model's own calls: a call that is
# exactly `reasoning("...")` on one plain string literal is answered by the app with
# `(no output)`, without approval and without reaching the REPL (see
# `is_reasoning_call` in src/tool_reasoning.rs). With `[chat] tool_reasoning` on, the
# app also shows earlier reasoning to the model as such calls, again without running
# them. Any other code that calls it (`reasoning(x)`, or alongside other statements)
# goes through approval and runs this body.
#
# This definition still matters: `help()` lists it, so the docstring is what the model
# reads about the function. Edit the docstring freely, but keep the body a no-op that
# prints nothing and returns None, or the app's `(no output)` answer becomes a lie.


@register
def reasoning(text):
    """Record your reasoning as part of the conversation. Has no other effect; returns None."""
    pass
