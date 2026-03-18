"""Shared fixtures for e2e tests. Runs against a deployed app via APP_HOST / APP_PORT."""

import os

import httpx
import pytest


@pytest.fixture
def base_url():
    host = os.environ.get("APP_HOST", "localhost")
    port = os.environ.get("APP_PORT", "8080")
    return f"http://{host}:{port}"


@pytest.fixture
def client(base_url):
    """HTTP client with 3s timeout — tests should fail fast, not hang."""
    with httpx.Client(base_url=base_url, timeout=3.0) as c:
        yield c
