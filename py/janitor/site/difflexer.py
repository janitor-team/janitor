#!/usr/bin/python3

from pygments.lexers.diff import DiffLexer as _DiffLexer
from pygments.token import Generic

TrailingWhitespace = Generic.Inserted.TrailingWhitespace


class DiffLexer(_DiffLexer):
    """Diff lexer that marks the trailing whitespace on added lines."""

    def get_tokens_unprocessed(self, text, stack=("root",)):
        for index, token, value in super().get_tokens_unprocessed(text, stack):
            end = len(value.rstrip(" \t"))
            if token is Generic.Inserted and end < len(value):
                yield index, token, value[:end]
                yield index + end, TrailingWhitespace, value[end:]
            else:
                yield index, token, value
