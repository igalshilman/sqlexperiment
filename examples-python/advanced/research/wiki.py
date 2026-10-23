"""
The researchers' tools: Wikipedia's search and articles, through its public
API (no key). A tool call's result is stored with the conversation, so a
retried attempt does not fetch it again.
"""

import httpx
from restate_agents import tool

API = "https://en.wikipedia.org/w/api.php"
# Wikipedia asks its API's clients to say who they are.
HEADERS = {
    "User-Agent": f"restate-agents-research-example/1.0 (https://github.com/restatedev) httpx/{httpx.__version__}"
}


async def _query(**params: str) -> dict:
    async with httpx.AsyncClient(headers=HEADERS, timeout=30) as client:
        response = await client.get(API, params={"action": "query", "format": "json", "formatversion": "2", **params})
        response.raise_for_status()
        return response.json()["query"]


@tool
async def search(query: str) -> list[str]:
    """Search Wikipedia: the titles of the five best matching articles, each with a snippet."""
    found = await _query(list="search", srsearch=query, srlimit="5")
    return [f"{hit['title']}: {hit['snippet']}" for hit in found["search"]]


@tool
async def read(title: str) -> str:
    """Read a Wikipedia article by its exact title: its text, at most 6,000 characters."""
    found = await _query(prop="extracts", explaintext="1", titles=title, redirects="1")
    [page] = found["pages"]
    return page.get("extract", f"No article titled {title!r}")[:6000]
