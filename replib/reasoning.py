# Must stay a no-op that prints nothing and returns None: with `[chat] tool_reasoning`
# on, the app shows earlier reasoning to the model as calls to this function with the
# result `(no output)`, without running them.


@register
def reasoning(text):
    """Record your reasoning as part of the conversation. Has no other effect; returns None."""
