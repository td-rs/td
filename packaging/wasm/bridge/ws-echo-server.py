#!/usr/bin/env python3
# Minimal WS echo server for the transport-primitive probe.
import asyncio
import websockets


async def echo(ws):
    async for msg in ws:
        await ws.send(msg)


async def main():
    async with websockets.serve(echo, '127.0.0.1', 8908):
        print('ws echo on 127.0.0.1:8908')
        await asyncio.Future()


asyncio.run(main())
