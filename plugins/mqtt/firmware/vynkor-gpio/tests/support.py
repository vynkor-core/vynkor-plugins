"""Shared host setup: import paths and one hand-driven MicroPython clock.

Every test module must use this clock; two modules patching time.ticks_ms
with separate clocks leave one of them reading a clock nobody advances.
"""

import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, os.path.join(ROOT, 'stubs'))
sys.path.insert(0, ROOT)

clock = [0]
time.ticks_ms = lambda: clock[0]
time.ticks_diff = lambda a, b: a - b
time.ticks_add = lambda a, b: a + b
