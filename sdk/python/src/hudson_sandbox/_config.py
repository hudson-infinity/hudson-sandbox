"""Trusted, private Unix configuration. No ambient proxy or bearer credentials."""

import os
from pathlib import Path
import ssl
import stat
from urllib.parse import urlsplit
from ._wire import ClientError, parse_json


def private_bytes(path):
    if os.name != "posix":
        raise ClientError("configuration")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as file:
            meta = os.fstat(file.fileno())
            if (
                not stat.S_ISREG(meta.st_mode)
                or meta.st_uid != os.geteuid()
                or meta.st_mode & 0o077
                or meta.st_size > 65536
            ):
                raise ClientError("configuration")
            data = file.read(65537)
            if len(data) > 65536:
                raise ClientError("configuration")
            return data
    except OSError:
        raise ClientError("configuration") from None


class Config:
    def __init__(self, path):
        try:
            data = parse_json(private_bytes(path))
            if (
                not isinstance(data, dict)
                or set(data)
                - {
                    "version",
                    "endpoint",
                    "credential_file",
                    "ca_file",
                    "request_timeout_seconds",
                }
                or type(data["version"]) is not int
                or data["version"] != 1
            ):
                raise ValueError()
            self.timeout = data.get("request_timeout_seconds", 30)
            if type(self.timeout) is not int or not 1 <= self.timeout <= 120:
                raise ValueError()
            endpoint = data["endpoint"]
            if not isinstance(endpoint, str) or any(
                ord(c) < 33 or 127 <= ord(c) <= 159 or c == "\\" for c in endpoint
            ):
                raise ValueError()
            url = urlsplit(endpoint)
            if (
                url.scheme != "https"
                or not url.hostname
                or url.username is not None
                or url.password is not None
                or url.path not in ("", "/")
                or url.query
                or url.fragment
                or "?" in endpoint
                or "#" in endpoint
            ):
                raise ValueError()
            _ = url.port  # Validate malformed/out-of-range ports before any request.
            self.endpoint = endpoint.rstrip("/")
            parent = Path(path).parent
            credential = parse_json(private_bytes(parent / data["credential_file"]))
            if (
                set(credential)
                != {
                    "version",
                    "project_id",
                    "name",
                    "token",
                    "created_at",
                    "expires_at",
                }
                or type(credential["version"]) is not int
                or credential["version"] != 1
            ):
                raise ValueError()
            if (
                not isinstance(credential["project_id"], str)
                or not credential["project_id"].startswith("prj_")
                or not isinstance(credential["name"], str)
                or len(credential["name"].encode()) > 4096
            ):
                raise ValueError()
            if (
                type(credential["created_at"]) is not int
                or type(credential["expires_at"]) is not int
                or not 0
                <= credential["created_at"]
                < credential["expires_at"]
                <= 2**63 - 1
            ):
                raise ValueError()
            self.token = credential["token"]
            if (
                not isinstance(self.token, str)
                or not 1 <= len(self.token) <= 4096
                or any(not 33 <= ord(c) <= 126 for c in self.token)
            ):
                raise ValueError()
            if data.get("ca_file") is not None:
                self.ssl = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
                self.ssl.minimum_version = ssl.TLSVersion.TLSv1_2
                self.ssl.load_verify_locations(
                    cadata=private_bytes(parent / data["ca_file"]).decode("ascii")
                )
            else:
                self.ssl = ssl.create_default_context()
        except (KeyError, TypeError, ValueError, OSError, ClientError):
            raise ClientError("configuration") from None

    def __repr__(self):
        return "Config(...)"
