"""The checks themselves."""

import datetime
import socket
import ssl

import httpx

from .config import Target

USER_AGENT = "lighthouse-probe/2"

# Warn this many days before a TLS certificate expires.
CERT_WARNING_DAYS = 21


def check_http(target: Target) -> tuple[bool, str]:
    """Up when the answer has a status below 400."""
    try:
        response = httpx.get(
            target.url,
            timeout=target.timeout,
            headers={"User-Agent": USER_AGENT},
            follow_redirects=True,
        )
    except httpx.HTTPError as e:
        return False, str(e)
    return response.status_code < 400, f"HTTP {response.status_code}"


def check_tls(host: str, port: int = 443) -> tuple[bool, str]:
    """Up when the certificate has more than CERT_WARNING_DAYS left."""
    context = ssl.create_default_context()
    with socket.create_connection((host, port), timeout=5) as sock:
        with context.wrap_socket(sock, server_hostname=host) as tls:
            cert = tls.getpeercert()
    expires = datetime.datetime.strptime(cert["notAfter"], "%b %d %H:%M:%S %Y %Z")
    left = (expires - datetime.datetime.utcnow()).days
    return left > CERT_WARNING_DAYS, f"{left} days left"
