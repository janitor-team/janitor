#!/usr/bin/python3

from pygments.lexers.diff import DiffLexer as _DiffLexer
from pygments.token import Generic

TrailingWhitespace = Generic.Inserted.TrailingWhitespace


class DiffLexer(_DiffLexer):
    """Diff lexer that marks the trailing whitespace on added lines."""

    def get_tokens_unprocessed(self, text, stack=("root",)):
        after_old_header = False
        for index, token, value in super().get_tokens_unprocessed(text, stack):
            # A "+++ " line right after a "--- " line is a file header.
            if (
                token is Generic.Inserted
                and value.startswith("+")
                and not (after_old_header and value.startswith("+++ "))
            ):
                end = len(value.rstrip(" \t"))
                if end < len(value):
                    after_old_header = False
                    yield index, token, value[:end]
                    yield index + end, TrailingWhitespace, value[end:]
                    continue
            if value != "\n":
                after_old_header = token is Generic.Deleted and value.startswith("--- ")
            yield index, token, value
