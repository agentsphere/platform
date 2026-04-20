"""Shopping cart backed by Valkey (Redis-compatible). Falls back to in-memory for local dev."""

import json
import logging

logger = logging.getLogger(__name__)

CART_TTL = 86400  # 24 hours


class Cart:
    """Abstract cart interface."""

    async def get_items(self, session_id: str) -> list[dict]:
        raise NotImplementedError

    async def add_item(self, session_id: str, product: dict, quantity: int = 1) -> list[dict]:
        raise NotImplementedError

    async def remove_item(self, session_id: str, product_id: int) -> list[dict]:
        raise NotImplementedError

    async def clear(self, session_id: str) -> None:
        raise NotImplementedError

    def _merge_item(self, items: list[dict], product: dict, quantity: int) -> list[dict]:
        """Add item or increment quantity if already in cart."""
        for item in items:
            if item["product_id"] == product["id"]:
                item["quantity"] += quantity
                return items
        items.append(
            {
                "product_id": product["id"],
                "name": product["name"],
                "price_cents": product["price_cents"],
                "quantity": quantity,
            }
        )
        return items


class ValkeyCart(Cart):
    """Cart stored in Valkey with JSON serialization."""

    def __init__(self, redis_client):
        self._r = redis_client

    def _key(self, session_id: str) -> str:
        return f"cart:{session_id}"

    async def get_items(self, session_id: str) -> list[dict]:
        data = await self._r.get(self._key(session_id))
        if data is None:
            return []
        return json.loads(data)

    async def _save(self, session_id: str, items: list[dict]) -> None:
        await self._r.set(self._key(session_id), json.dumps(items), ex=CART_TTL)

    async def add_item(self, session_id: str, product: dict, quantity: int = 1) -> list[dict]:
        items = await self.get_items(session_id)
        items = self._merge_item(items, product, quantity)
        await self._save(session_id, items)
        return items

    async def remove_item(self, session_id: str, product_id: int) -> list[dict]:
        items = await self.get_items(session_id)
        items = [i for i in items if i["product_id"] != product_id]
        await self._save(session_id, items)
        return items

    async def clear(self, session_id: str) -> None:
        await self._r.delete(self._key(session_id))


class MemoryCart(Cart):
    """In-memory fallback for local dev without Valkey."""

    def __init__(self):
        self._store: dict[str, list[dict]] = {}

    async def get_items(self, session_id: str) -> list[dict]:
        return list(self._store.get(session_id, []))

    async def add_item(self, session_id: str, product: dict, quantity: int = 1) -> list[dict]:
        items = list(self._store.get(session_id, []))
        items = self._merge_item(items, product, quantity)
        self._store[session_id] = items
        return items

    async def remove_item(self, session_id: str, product_id: int) -> list[dict]:
        items = list(self._store.get(session_id, []))
        items = [i for i in items if i["product_id"] != product_id]
        self._store[session_id] = items
        return items

    async def clear(self, session_id: str) -> None:
        self._store.pop(session_id, None)
