OUT = 1
IN = 0

# GPIO number -> level. Tests write here to fake an input.
levels = {}


class Pin:
    OUT = 1
    IN = 0
    PULL_UP = 2

    def __init__(self, num, mode=None, pull=None, value=None):
        self.num = num
        if value is not None:
            levels[num] = value

    def value(self, v=None):
        if v is None:
            return levels.get(self.num, 0)
        levels[self.num] = v


class PWM:
    def __init__(self, pin, freq=1000, duty=0):
        self.pin = pin
        self._f = freq
        self._d = duty

    def freq(self, f=None):
        if f is None:
            return self._f
        self._f = f

    def duty(self, d=None):
        if d is None:
            return self._d
        self._d = d

    def deinit(self):
        pass


class ADC:
    reading = 0

    def __init__(self, source):
        self.source = source

    def read_u16(self):
        return ADC.reading


class WDT:
    feeds = 0

    def feed(self):
        WDT.feeds += 1


def reset():
    raise SystemExit('machine.reset()')
