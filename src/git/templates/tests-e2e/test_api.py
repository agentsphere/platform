"""Smoke test: verify the app responds on its root endpoint."""


def test_root_returns_success(client):
    """The app should respond on / with a 2xx or redirect."""
    resp = client.get("/", follow_redirects=False)
    assert resp.status_code in (200, 301, 302, 307, 308)
