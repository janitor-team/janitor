import re

from pygments.filter import Filter
from pygments.token import Generic

# highlight_diff shows each carriage return as this symbol
CARRIAGE_RETURN = "\N{SYMBOL FOR CARRIAGE RETURN}"


class TrailingWhitespaceFilter(Filter):
    """Mark the trailing whitespace on added and removed diff lines.

    A carriage return inside a line gets a token of its own.
    """

    def filter(self, lexer, stream):
        for ttype, value in stream:
            end = len(value.rstrip(" \t" + CARRIAGE_RETURN))
            for part in re.split(f"({CARRIAGE_RETURN})", value[:end]):
                if part:
                    yield ttype, part
            if end < len(value):
                if ttype in (Generic.Inserted, Generic.Deleted):
                    ttype = ttype.TrailingWhitespace
                yield ttype, value[end:]
