"""Widget module for the Python scan fixture."""
from __future__ import annotations

import os
from collections import OrderedDict

LIMIT = 7
NAME: str = "basic"


class Base:
    def describe(self) -> str:
        return "base"


class Widget(Base):
    def __init__(self, value: int) -> None:
        self.value = value

    def run(self) -> int:
        return helper(self.value)


def helper(value: int) -> int:
    return value


def answer() -> int:
    return Widget(42).run()


def test_widget_runs():
    assert Widget(1).run() == 1


class TestWidget:
    def test_answer(self):
        assert answer() == LIMIT
