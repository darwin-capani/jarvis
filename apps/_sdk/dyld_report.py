#!/usr/bin/env python3
"""Reference dyld module-report stub for JARVIS micro-apps (docs/INTROSPECT.md).

A micro-app collects its loaded-module inventory once (e.g. on `start`) and sends
it to jarvisd over the EXISTING per-app socket as a
`{"type":"modules","data":{"modules":[{"path","uuid"},...]}}` line (the app's own
`send` still stamps the capability token). The daemon attests the set against a
trust-on-first-use baseline and flags any module the baseline never had —
injection / unexpected dlopen — see daemon/src/introspect.rs.

COOPERATIVE + READ-ONLY. This enumerates the app's OWN dyld image list IN-PROCESS
via the public `_dyld_*` C API (no entitlement, no task_for_pid, no ptrace). It is
a self-report: reliable against injection into an otherwise-honest app, and an
auditable inventory — NOT a defense against an app that lies about itself (that
deeper compromise is bounded by the sandbox + per-launch token). macOS-only; on
any error it degrades to an empty/partial list and NEVER raises into the caller.

Usage (an app whose send takes a full object, e.g. example-plugin):
    send(conn, {"type": "modules", "data": dyld_report.modules_payload()})
Usage (an app whose send takes (type, data), e.g. global-scan):
    bridge.send("modules", dyld_report.modules_payload())
"""
import ctypes
import struct
import sys
import threading
import uuid

_LC_UUID = 0x1B
_MH_MAGIC_64 = 0xFEEDFACF
_MAX_IMAGES = 8192  # bound a pathological process; mirrors the daemon's MAX_MODULES


def _image_uuid(header_addr):
    """Parse LC_UUID out of the 64-bit Mach-O header mapped at header_addr (the
    app's OWN in-process image header), or None if absent/not-64-bit/malformed."""
    try:
        # mach_header_64 = magic,cputype,cpusubtype,filetype,ncmds,sizeofcmds,flags,reserved
        hdr = ctypes.string_at(header_addr, 32)
        magic, _cput, _cpus, _ftype, ncmds, sizeofcmds = struct.unpack("<IiiIII", hdr[:24])
        if magic != _MH_MAGIC_64 or ncmds == 0 or not (0 < sizeofcmds <= (1 << 20)):
            return None
        cmds = ctypes.string_at(header_addr + 32, sizeofcmds)
        off = 0
        for _ in range(ncmds):
            if off + 8 > len(cmds):
                break
            cmd, cmdsize = struct.unpack_from("<II", cmds, off)
            if cmdsize < 8 or off + cmdsize > len(cmds):
                break
            if cmd == _LC_UUID and off + 24 <= len(cmds):
                return str(uuid.UUID(bytes=cmds[off + 8 : off + 24])).upper()
            off += cmdsize
    except Exception:  # noqa: BLE001 — attestation must never raise into the app
        return None
    return None


def collect_loaded_modules():
    """Return [{"path": str, "uuid": str|None}, ...] for every loaded dyld image.
    Empty list on non-macOS or any failure."""
    if sys.platform != "darwin":
        return []
    try:
        d = ctypes.CDLL(None)  # _dyld_* live in libSystem, already loaded
        d._dyld_image_count.restype = ctypes.c_uint32
        d._dyld_get_image_name.restype = ctypes.c_char_p
        d._dyld_get_image_name.argtypes = [ctypes.c_uint32]
        d._dyld_get_image_header.restype = ctypes.c_void_p
        d._dyld_get_image_header.argtypes = [ctypes.c_uint32]
        n = min(int(d._dyld_image_count()), _MAX_IMAGES)
    except Exception:  # noqa: BLE001
        return []
    out = []
    for i in range(n):
        try:
            name = d._dyld_get_image_name(i)
            if not name:
                continue
            hdr = d._dyld_get_image_header(i)
            out.append(
                {
                    "path": name.decode("utf-8", "replace"),
                    "uuid": _image_uuid(hdr) if hdr else None,
                }
            )
        except Exception:  # noqa: BLE001
            continue
    return out


def modules_payload():
    """The `data` object for a `{"type":"modules"}` report line."""
    return {"modules": collect_loaded_modules()}


# --- live dlopen watch (optional) ------------------------------------------
#
# A one-shot startup report catches the load set at launch (incl. any DYLD_INSERT
# injection, which happens at launch). To also catch a RUNTIME dlopen, register a
# dyld add-image callback that flips a thread-safe flag; the app then re-sends a
# fresh report from its OWN thread when the flag is set. The callback deliberately
# does NOT touch the socket — it only sets an event — so there is no cross-thread
# socket write and no reentrancy into the app's I/O.

# _dyld_register_func_for_add_image callback: void(const mach_header*, intptr_t).
_ADD_IMAGE_CB = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_ssize_t)
_changed = threading.Event()
_add_cb = None  # keep the CFUNCTYPE alive — GC'ing it would crash on the next call


def _on_add_image(_header, _slide):  # pragma: no cover - fires on dlopen only
    try:
        _changed.set()
    except Exception:
        pass


def watch():
    """Register a dyld add-image callback so a LATER dlopen sets a 'changed' flag.
    Idempotent, macOS-only, never raises. Returns True iff watching is active.

    NOTE: registration fires the callback ONCE for every already-loaded image
    (before returning), so we clear the flag immediately after — only genuine
    dlopens AFTER this call leave it set."""
    global _add_cb
    if sys.platform != "darwin" or _add_cb is not None:
        return _add_cb is not None
    try:
        d = ctypes.CDLL(None)
        d._dyld_register_func_for_add_image.argtypes = [_ADD_IMAGE_CB]
        d._dyld_register_func_for_add_image.restype = None
        _add_cb = _ADD_IMAGE_CB(_on_add_image)
        d._dyld_register_func_for_add_image(_add_cb)  # fires for existing images
        _changed.clear()  # discard the initial bulk; watch only future dlopens
        return True
    except Exception:  # noqa: BLE001
        _add_cb = None
        return False


def modules_changed_and_clear():
    """True (and resets the flag) iff an image was loaded since the last check or
    since watch(). False if not watching or nothing changed."""
    if _changed.is_set():
        _changed.clear()
        return True
    return False


if __name__ == "__main__":
    # Manual probe: print this process's own module inventory.
    import json

    print(json.dumps(modules_payload(), indent=2))
