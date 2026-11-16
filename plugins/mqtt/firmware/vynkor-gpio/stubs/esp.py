import os

SLOT = '/tmp/espfw/fake_esp.bin'
BASE = 0x3FB000


def flash_read(addr, size):
    try:
        with open(SLOT, 'rb') as f:
            data = f.read()
    except FileNotFoundError:
        data = b''
    off = addr - BASE
    chunk = data[off:off + size]
    return chunk + b'\xff' * (size - len(chunk))


def flash_erase(addr):
    try:
        os.remove(SLOT)
    except FileNotFoundError:
        pass


def flash_write(addr, buf):
    with open(SLOT, 'wb') as f:
        f.write(bytes(buf))
