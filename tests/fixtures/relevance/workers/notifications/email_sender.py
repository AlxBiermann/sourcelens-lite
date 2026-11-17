"""Transactional emails: order confirmations and shipping updates."""

import smtplib
from email.message import EmailMessage
from string import Template

TEMPLATES = {
    "order_confirmation": Template(
        "Hi $name,\n\nthanks for your order $order_id. We will email you again when it ships.\n"
    ),
    "shipping_update": Template(
        "Hi $name,\n\nyour order $order_id is on its way. Track it at $tracking_url.\n"
    ),
}


class SmtpClient:
    """Thin wrapper around smtplib that reconnects once when the server dropped the connection."""

    def __init__(self, host: str, port: int, username: str, password: str):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self._conn: smtplib.SMTP | None = None

    def send(self, message: EmailMessage) -> None:
        try:
            self._connection().send_message(message)
        except smtplib.SMTPServerDisconnected:
            self._conn = None
            self._connection().send_message(message)

    def _connection(self) -> smtplib.SMTP:
        if self._conn is None:
            self._conn = smtplib.SMTP(self.host, self.port, timeout=10)
            self._conn.starttls()
            self._conn.login(self.username, self.password)
        return self._conn


def render_email_template(template_name: str, **values: str) -> str:
    """Fills a named template; a missing placeholder value raises KeyError."""
    return TEMPLATES[template_name].substitute(values)


def send_order_confirmation(client: SmtpClient, order: dict, customer: dict) -> None:
    """Emails the customer that the order was received, with the order number."""
    message = EmailMessage()
    message["From"] = "Shopflow <orders@shopflow.example>"
    message["To"] = customer["email"]
    message["Subject"] = f"Your order {order['id']}"
    message.set_content(
        render_email_template(
            "order_confirmation", name=customer["name"], order_id=order["id"]
        )
    )
    client.send(message)
