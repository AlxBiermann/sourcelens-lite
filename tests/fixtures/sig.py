import math


def foo(a, b) -> int:
    """Return the rounded hypotenuse."""
    total = a * a + b * b
    return int(math.sqrt(total))
