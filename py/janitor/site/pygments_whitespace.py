from pygments.filter import Filter
from pygments.token import Generic

# highlight_diff shows each carriage return as this symbol
CARRIAGE_RETURN = "\N{SYMBOL FOR CARRIAGE RETURN}"


class TrailingWhitespaceFilter(Filter):
    """Mark the trailing whitespace on added and removed diff lines."""

    def filter(self, lexer, stream):
        for ttype, value in stream:
            end = len(value.rstrip(" \t" + CARRIAGE_RETURN))
            if ttype in (Generic.Inserted, Generic.Deleted) and end < len(value):
                yield ttype, value[:end]
                yield ttype.TrailingWhitespace, value[end:]
            else:
                yield ttype, value
