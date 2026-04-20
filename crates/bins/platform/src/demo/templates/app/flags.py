"""Feature flag client — reads from platform API."""

import os
import httpx

PLATFORM_API_URL = os.getenv("PLATFORM_API_URL", "")
PLATFORM_API_TOKEN = os.getenv("PLATFORM_API_TOKEN", "")
PLATFORM_PROJECT_ID = os.getenv("PLATFORM_PROJECT_ID", "")


async def evaluate(keys: list[str], user_id: str | None = None) -> dict:
    """Evaluate feature flags via the platform API."""
    if not PLATFORM_API_URL or not PLATFORM_API_TOKEN or not PLATFORM_PROJECT_ID:
        return {k: False for k in keys}

    try:
        async with httpx.AsyncClient(timeout=2.0) as client:
            resp = await client.post(
                f"{PLATFORM_API_URL}/api/flags/evaluate",
                headers={"Authorization": f"Bearer {PLATFORM_API_TOKEN}"},
                json={
                    "project_id": PLATFORM_PROJECT_ID,
                    "keys": keys,
                    "user_id": user_id,
                },
            )
            if resp.status_code == 200:
                return resp.json().get("values", {})
    except Exception:
        pass

    return {k: False for k in keys}
