"""Observe a private classic Win32 console with simulated key events."""

import ctypes
from ctypes import wintypes as win
import subprocess
import time


class Coord(ctypes.Structure):
    _fields_ = [("x", ctypes.c_short), ("y", ctypes.c_short)]


class Key(ctypes.Structure):
    _fields_ = [("down", win.BOOL), ("repeat", win.WORD), ("code", win.WORD),
                ("scan", win.WORD), ("character", win.WCHAR), ("modifiers", win.DWORD)]


class Record(ctypes.Structure):
    _fields_ = [("kind", win.WORD), ("key", Key)]


def windows_console(cwd, env, examples, templates):
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetStdHandle.argtypes = [win.DWORD]
    kernel.GetStdHandle.restype = win.HANDLE
    kernel.GetConsoleMode.argtypes = [win.HANDLE, ctypes.POINTER(win.DWORD)]
    kernel.SetConsoleCursorPosition.argtypes = [win.HANDLE, Coord]
    kernel.FillConsoleOutputCharacterW.argtypes = [
        win.HANDLE, win.WCHAR, win.DWORD, Coord, ctypes.POINTER(win.DWORD)]
    kernel.ReadConsoleOutputCharacterW.argtypes = [
        win.HANDLE, ctypes.POINTER(win.WCHAR), win.DWORD, Coord, ctypes.POINTER(win.DWORD)]
    kernel.WriteConsoleInputW.argtypes = [
        win.HANDLE, ctypes.POINTER(Record), win.DWORD, ctypes.POINTER(win.DWORD)]

    kernel.FreeConsole()
    if not kernel.AllocConsole():
        print(f"classic Win32 console not run: AllocConsole winerror {ctypes.get_last_error()}", flush=True)
        return

    def checked(success):
        if not success:
            raise ctypes.WinError(ctypes.get_last_error())

    def key_in(text):
        events = (Record * len(text))()
        for i, character in enumerate(text):
            events[i].kind = 1
            if character == "\r":
                events[i].key = Key(True, 1, 13, 28, character, 0)
            else:
                events[i].key = Key(True, 1, 0, 0, character, 0)
        count = win.DWORD()
        checked(kernel.WriteConsoleInputW(stdin, events, len(text), ctypes.byref(count)))
        assert count.value == len(text)

    def read_screen():
        text = ctypes.create_unicode_buffer(4096)
        count = win.DWORD()
        checked(kernel.ReadConsoleOutputCharacterW(stdout, text, len(text), Coord(0, 0),
                                                   ctypes.byref(count)))
        return text[:count.value]

    def expect(token, child):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            screen = read_screen()
            if token in screen:
                return screen
            if child.poll() is not None:
                raise RuntimeError((token, screen.rstrip(), child.returncode))
            time.sleep(0.02)
        raise TimeoutError((token, screen.rstrip()))

    def exercise(command, is_template):
        count = win.DWORD()
        checked(kernel.FillConsoleOutputCharacterW(stdout, " ", 4096, Coord(0, 0),
                                                   ctypes.byref(count)))
        checked(kernel.SetConsoleCursorPosition(stdout, Coord(0, 0)))
        child = subprocess.Popen(command, cwd=cwd, env=env)
        try:
            if is_template:
                first = expect("我的名字是", child)
                assert "，今年" not in first and child.poll() is None
                key_in("小明\r")
                middle = expect("，今年", child)
                assert "小明" in middle and "岁。" not in middle and child.poll() is None
                key_in("0018\r")
                expect("岁。", child)
                assert child.wait(timeout=5) == 0
                screen = read_screen()
                assert screen.count("小明") >= 2 and "0018" in screen, screen.rstrip()
            else:
                first = expect("请输入名字：", child)
                assert "你好，小明！" not in first and child.poll() is None
                key_in("小明\r")
                expect("你好，小明！", child)
                assert child.wait(timeout=5) == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()

    try:
        stdin = kernel.GetStdHandle(-10)
        stdout = kernel.GetStdHandle(-11)
        for handle in (stdin, stdout):
            checked(kernel.GetConsoleMode(handle, ctypes.byref(win.DWORD())))
        checked(kernel.SetConsoleCP(65001))
        checked(kernel.SetConsoleOutputCP(65001))
        for (mode, example), (_, template) in zip(examples, templates, strict=True):
            exercise(example, False)
            exercise(template, True)
            print(f"{mode}: real classic Win32 console prompt and segmented template; simulated keys", flush=True)
    finally:
        kernel.FreeConsole()
