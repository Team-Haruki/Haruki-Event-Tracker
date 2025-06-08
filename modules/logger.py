import asyncio
import logging
import coloredlogs
from typing import Optional

LOG_FORMAT = "[%(asctime)s][%(levelname)s][%(name)s] %(message)s"
FIELD_STYLE = {
    "asctime": {"color": "green"},
    "levelname": {"color": "blue", "bold": True},
    "name": {"color": "magenta"},
    "message": {"color": 144, "bright": False},
}


class AsyncLogger(object):
    def __init__(self, name: str = "async-logger", level: str = "INFO", queue_size: int = 1000) -> None:
        self._sync_logger = logging.getLogger(f"{name}-sync")
        coloredlogs.install(level=level, logger=self._sync_logger, fmt=LOG_FORMAT, field_styles=FIELD_STYLE)
        self.logger = logging.getLogger(name)
        self.logger.setLevel(level)
        self.queue: asyncio.Queue[Optional[logging.LogRecord]] = asyncio.Queue(maxsize=queue_size)
        self.logger.handlers.clear()
        self.logger.addHandler(_AsyncQueueHandler(self.queue))

        self._task: Optional[asyncio.Task] = None

    async def start(self) -> None:
        if not self._task or self._task.done():
            self._task = asyncio.create_task(self._log_worker())

    async def stop(self) -> None:
        if self._task:
            await self.queue.put(None)
            await self._task
            self._task = None

    async def _log_worker(self) -> None:
        while True:
            record = await self.queue.get()
            if record is None:
                break
            self._sync_logger.handle(record)

    async def debug(self, *args, **kwargs) -> None:
        self.logger.debug(*args, **kwargs)

    async def info(self, *args, **kwargs) -> None:
        self.logger.info(*args, **kwargs)

    async def warning(self, *args, **kwargs) -> None:
        self.logger.warning(*args, **kwargs)

    async def error(self, *args, **kwargs) -> None:
        self.logger.error(*args, **kwargs)

    async def critical(self, *args, **kwargs) -> None:
        self.logger.critical(*args, **kwargs)

    async def exception(self, *args, **kwargs) -> None:
        self.logger.exception(*args, **kwargs)


class _AsyncQueueHandler(logging.Handler):
    def __init__(self, queue: asyncio.Queue) -> None:
        super().__init__()
        self.queue = queue

    def emit(self, record: logging.LogRecord) -> None:
        try:
            self.queue.put_nowait(record)
        except asyncio.QueueFull:
            pass
