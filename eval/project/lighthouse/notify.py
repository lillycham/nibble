"""Telling someone that a target went down or came back."""

import smtplib
from email.message import EmailMessage

import httpx

NTFY_SERVER = "https://ntfy.sh"


def send_email(settings: dict, subject: str, body: str) -> None:
    message = EmailMessage()
    message["Subject"] = subject
    message["From"] = settings["from"]
    message["To"] = settings["to"]
    message.set_content(body)
    with smtplib.SMTP(settings.get("smtp_host", "localhost")) as smtp:
        smtp.send_message(message)


def send_matrix(settings: dict, subject: str, body: str) -> None:
    url = f"{settings['homeserver']}/_matrix/client/v3/rooms/{settings['room']}/send/m.room.message"
    httpx.post(
        url,
        headers={"Authorization": f"Bearer {settings['access_token']}"},
        json={"msgtype": "m.text", "body": f"{subject}\n{body}"},
    )


def send_ntfy(settings: dict, subject: str, body: str) -> None:
    server = settings.get("server", NTFY_SERVER)
    httpx.post(f"{server}/{settings['topic']}", headers={"Title": subject}, content=body)


BACKENDS = {
    "email": send_email,
    "matrix": send_matrix,
    "ntfy": send_ntfy,
}


def notify(settings: dict, subject: str, body: str) -> None:
    BACKENDS[settings["backend"]](settings, subject, body)
