from lighthouse import checks
from lighthouse.config import Target


def test_http_up(httpserver):
    httpserver.expect_request("/").respond_with_data("ok")
    up, detail = checks.check_http(Target("local", httpserver.url_for("/")))
    assert up and detail == "HTTP 200"


def test_http_down(httpserver):
    httpserver.expect_request("/").respond_with_data("no", status=503)
    up, _ = checks.check_http(Target("local", httpserver.url_for("/")))
    assert not up


def test_tls_warning_days():
    assert checks.CERT_WARNING_DAYS == 21
