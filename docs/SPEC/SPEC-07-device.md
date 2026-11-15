# device：USB 音频

状态：待定。2026-11-15 起为待办清单，记法见 [SPEC.md](SPEC.md)。

普通 USB 声卡的枚举与使用已随 SPEC-04 定稿落地（枚举归 daemon，ALSA API，CLI `device list`）。本文件仅剩 usb-gadget/UAC 路径待定。

## 待办

- [ ] 待确认：是否保留 usb-gadget/UAC 路径；保留时放在 daemon 还是独立进程。整块未动，全仓无 gadget 代码。
