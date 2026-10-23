"""
The systems the supervisor works with, as plain Restate services: stand-ins
for your inventory, advisories, ticketing, code host and pager. Each step
that reaches the outside is a ``ctx.run``, stored once: a retried call does
not open a second ticket.
"""

import random

import restate_fns as restate
from pydantic import BaseModel

inventory = restate.Service("inventory")


@inventory.handler()
async def describe(ctx: restate.Context, service: str) -> str:
    """A service's owner, its runtime, and the versions of its dependencies."""
    services = {
        "checkout": "owner: payments team; runtime: python 3.12; depends on: requests 2.25.1, jinja2 3.1.2",
        "search": "owner: discovery team; runtime: node 22; depends on: express 4.17.1, lodash 4.17.20",
    }
    return services.get(service, f"no service {service}")


advisories = restate.Service("advisories")


@advisories.handler()
async def lookup(ctx: restate.Context, package: str) -> str:
    """The advisories of a package: what is affected, and the first version fixed."""
    known = {
        "requests": "CVE-2023-32681: leaks Proxy-Authorization on redirects; versions < 2.31.0, fixed in 2.31.0",
        "lodash": "CVE-2021-23337: command injection in template; versions < 4.17.21, fixed in 4.17.21",
    }
    return known.get(package, f"no advisories for {package}")


class Ticket(BaseModel):
    service: str
    title: str
    body: str


tickets = restate.Service("tickets")


@tickets.handler()
async def open_ticket(ctx: restate.Context, ticket: Ticket) -> str:
    """Open a ticket: its id."""

    def create() -> str:  # a stand-in for your tracker's API, which answers with the ticket's id
        print(f"ticket for {ticket.service}: {ticket.title}")
        return f"SEC-{random.randint(1000, 9999)}"

    return await ctx.run_typed("open the ticket", create)


class Change(BaseModel):
    service: str
    title: str
    diff: str


repo = restate.Service("repo")


@repo.handler()
async def open_pull_request(ctx: restate.Context, change: Change) -> str:
    """Open a pull request with the change: its URL."""

    def create() -> str:  # a stand-in for your code host's API
        print(f"pull request for {change.service}:\n{change.diff}")
        return f"https://git.example.com/{change.service}/pulls/{random.randint(100, 999)}"

    return await ctx.run_typed("open the pull request", create)


pager = restate.Service("pager")


@pager.handler()
async def page(ctx: restate.Context, message: str) -> None:
    """Page the security engineer on call."""
    await ctx.run_typed("page", lambda: print(f"PAGE: {message}"))
