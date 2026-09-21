#!/usr/bin/env python3
"""Read-only development-host preflight; a passing result is not VM isolation proof."""
import json
import os
import platform
import sys
from pathlib import Path


def inspect_host():
    result = {
        "system": platform.system(),
        "architecture": platform.machine(),
        "kernel": platform.release(),
        "kvm_api_version": None,
        "cgroup_controllers": [],
        "errors": [],
        "scope": "development preflight; no workload or isolation tests executed",
    }
    if result["system"] != "Linux":
        result["errors"].append("Run inside the Linux development VM or on the Linux compute host")
        return result
    if result["architecture"] not in ("aarch64", "x86_64"):
        result["errors"].append("Unsupported development architecture")
    try:
        import fcntl
        # KVM_GET_API_VERSION is _IO(KVMIO, 0x00); the stable API version is 12.
        # Open read/write as required by the KVM API, without creating a VM.
        fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
        try:
            result["kvm_api_version"] = fcntl.ioctl(fd, 0xAE00, 0)
        finally:
            os.close(fd)
        if result["kvm_api_version"] != 12:
            result["errors"].append("Unsupported KVM API version")
    except OSError as error:
        result["errors"].append(f"Cannot access the KVM API (errno {error.errno})")
    try:
        controllers = Path("/sys/fs/cgroup/cgroup.controllers").read_text().split()
        result["cgroup_controllers"] = sorted(controllers)
        missing = sorted({"cpu", "memory", "pids"}.difference(controllers))
        if missing:
            result["errors"].append("Missing cgroup v2 controllers: " + ", ".join(missing))
    except OSError:
        result["errors"].append("No readable cgroup v2 root at /sys/fs/cgroup")
    return result


if __name__ == "__main__":
    report = inspect_host()
    print(json.dumps(report, indent=2, sort_keys=True))
    sys.exit(1 if report["errors"] else 0)
