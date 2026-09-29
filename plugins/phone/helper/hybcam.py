#!/usr/bin/python3
"""Camera preview frames via the libhybris camera compat layer (libcamera.so.1).
No window, no Mir surface, no shutter click. Deployed by the `phone` plugin's
phone_setup as hybcam-<sha8>.py.

Frames go to --out FILE or stdout ('-'); log/stats go to stderr.
  --fmt raw|y|jpeg      NV21 / luma only / JPEG (cv2 from ~/pylibs)
  --framing none|len32  len32 = 4-byte big-endian length before every frame
  --snap                emit ONE frame after autofocus settled, then exit
Exit codes: 0 ok, 2 connect failed, 3 no frames.
"""
import argparse
import ctypes as C
import os
import signal
import struct
import sys
import time

ap = argparse.ArgumentParser()
ap.add_argument("--cam", default="back", choices=["back", "front"])
ap.add_argument("-W", type=int, default=640)
ap.add_argument("-H", type=int, default=480)
ap.add_argument("--fps", type=int, default=15)
ap.add_argument("--secs", type=float, default=5, help="hard deadline (dead-man switch)")
ap.add_argument("--out", default="-")
ap.add_argument("--fmt", default="raw", choices=["raw", "y", "jpeg"])
ap.add_argument("--framing", default="none", choices=["none", "len32"])
ap.add_argument("--q", type=int, default=70, help="jpeg quality")
ap.add_argument("--af", default="", help="off|video|auto|macro|picture|infinity")
ap.add_argument("--flash", default="", help="0 off, 1 auto, 2 on, 3 torch")
ap.add_argument("--snap", action="store_true")
ap.add_argument("--settle", type=float, default=1.5, help="--snap: seconds after first frame")
ap.add_argument("--dump", action="store_true")
a = ap.parse_args()

if a.fmt == "jpeg":
    sys.path.insert(0, os.path.expanduser("~/pylibs"))
    import numpy as np
    import cv2

W, H = a.W, a.H
STOP = {"flag": False}


def log(*x):
    print(*x, file=sys.stderr, flush=True)


def on_signal(signum, frame):
    STOP["flag"] = True


for s in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
    signal.signal(s, on_signal)

lib = C.CDLL("libcamera.so.1")
VOIDP = C.c_void_p
CB_CTX = C.CFUNCTYPE(None, VOIDP)
CB_ZOOM = C.CFUNCTYPE(None, VOIDP, C.c_int32)
CB_DATA = C.CFUNCTYPE(None, VOIDP, C.c_uint32, VOIDP)
CB_SIZE = C.CFUNCTYPE(None, VOIDP, C.c_int, C.c_int)


class Listener(C.Structure):
    _fields_ = [
        ("on_msg_error_cb", CB_CTX), ("on_msg_shutter_cb", CB_CTX), ("on_msg_focus_cb", CB_CTX),
        ("on_msg_zoom_cb", CB_ZOOM), ("on_data_raw_image_cb", CB_DATA),
        ("on_data_compressed_image_cb", CB_DATA), ("on_preview_texture_needs_update_cb", CB_CTX),
        ("context", VOIDP), ("on_preview_frame_cb", CB_DATA),
    ]


fd = 1 if a.out == "-" else os.open(a.out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
st = {"n": 0, "t0": None, "last_raw": None}


def emit(buf):
    if a.framing == "len32":
        buf = struct.pack(">I", len(buf)) + bytes(buf)
    mv = memoryview(buf)
    try:
        while mv:
            w = os.write(fd, mv)
            mv = mv[w:]
    except (BrokenPipeError, OSError):
        STOP["flag"] = True


def encode(raw):
    if a.fmt == "raw":
        return bytes(raw)
    if a.fmt == "y":
        return bytes(raw[: W * H])
    yuv = np.frombuffer(raw, dtype=np.uint8).reshape(H * 3 // 2, W)
    bgr = cv2.cvtColor(yuv, cv2.COLOR_YUV2BGR_NV21)
    ok, enc = cv2.imencode(".jpg", bgr, [cv2.IMWRITE_JPEG_QUALITY, a.q])
    return enc.tobytes()


def on_frame(data, size, ctx):
    if st["t0"] is None:
        st["t0"] = time.time()
    st["n"] += 1
    raw = (C.c_char * size).from_address(data)
    if a.snap:
        st["last_raw"] = bytes(raw)
        return
    emit(encode(raw))


keep = [
    CB_CTX(lambda c: log("ERROR cb")), CB_CTX(lambda c: log("SHUTTER cb")), CB_CTX(lambda c: None),
    CB_ZOOM(lambda c, z: None), CB_DATA(lambda d, s, c: None), CB_DATA(lambda d, s, c: None),
    CB_CTX(lambda c: None), CB_DATA(on_frame),
]
lst = Listener(*keep[:7], None, keep[7])

lib.android_camera_connect_to.restype = VOIDP
lib.android_camera_connect_to.argtypes = [C.c_int, C.POINTER(Listener)]
ctl = lib.android_camera_connect_to(0 if a.cam == "back" else 1, C.byref(lst))
if not ctl:
    log("connect FAILED")
    sys.exit(2)


def release():
    try:
        lib.android_camera_stop_preview.argtypes = [VOIDP]
        lib.android_camera_stop_preview(ctl)
        lib.android_camera_disconnect.argtypes = [VOIDP]
        lib.android_camera_disconnect(ctl)
    except Exception as e:  # never mask the real exit path
        log("release error", e)


try:
    if a.dump:
        lib.android_camera_dump_parameters.argtypes = [VOIDP]
        lib.android_camera_dump_parameters(ctl)

    lib.android_camera_set_preview_size.argtypes = [VOIDP, C.c_int, C.c_int]
    lib.android_camera_set_preview_size(ctl, W, H)
    lib.android_camera_set_preview_format.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_format(ctl, 1)  # CAMERA_PIXEL_FORMAT_YUV420SP (NV21)
    lib.android_camera_set_preview_fps.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_fps(ctl, a.fps)

    if a.af:
        modes = {"off": 0, "video": 1, "auto": 2, "macro": 3, "picture": 4, "infinity": 5}
        lib.android_camera_set_auto_focus_mode.argtypes = [VOIDP, C.c_int]
        lib.android_camera_set_auto_focus_mode(ctl, modes[a.af])
    if a.flash:
        lib.android_camera_set_flash_mode.argtypes = [VOIDP, C.c_int]
        lib.android_camera_set_flash_mode(ctl, int(a.flash))

    # Without a preview target Camera2Client parks in WAITING_FOR_PREVIEW_WINDOW and
    # delivers nothing. GLConsumer only remembers the texture id until
    # updateTexImage(), which we never call, so a dummy id needs no GL context.
    lib.android_camera_set_preview_texture.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_texture(ctl, 1)
    lib.android_camera_set_preview_callback_mode.argtypes = [VOIDP, C.c_int]
    lib.android_camera_set_preview_callback_mode(ctl, 1)
    lib.android_camera_start_preview.argtypes = [VOIDP]
    lib.android_camera_start_preview(ctl)

    if a.af == "auto":
        time.sleep(1)
        lib.android_camera_start_autofocus.argtypes = [VOIDP]
        lib.android_camera_start_autofocus(ctl)

    deadline = time.time() + a.secs
    while not STOP["flag"] and time.time() < deadline:
        if a.snap and st["t0"] is not None and time.time() - st["t0"] >= a.settle and st["last_raw"]:
            emit(encode(st["last_raw"]))
            break
        time.sleep(0.05)
finally:
    release()

if a.snap and st["n"] == 0:
    log("no frames")
    os._exit(3)
if not a.snap:
    log("frames", st["n"])
os._exit(0)
