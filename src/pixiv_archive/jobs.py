"""Bounded work concurrency; SQLite writes and network admission are coordinated."""
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait

from .net import RemoteStop


def download_many(archive, refs, jobs=1):
    if not 1 <= jobs <= 4:
        raise ValueError("jobs must be between 1 and 4")
    if jobs == 1:
        for ref in refs:
            try:
                status = archive.save(ref)
            except RemoteStop:
                yield ref, "paused"
                raise
            yield ref, status
        return
    iterator = iter(refs)
    stopped = None
    with ThreadPoolExecutor(max_workers=jobs, thread_name_prefix="pixiv-work") as pool:
        pending = {}

        def fill():
            while not stopped and len(pending) < jobs:
                ref = next(iterator, None)
                if ref is None:
                    break
                pending[pool.submit(archive.save, ref)] = ref

        try:
            fill()
            while pending:
                completed, _ = wait(pending, return_when=FIRST_COMPLETED)
                for future in completed:
                    ref = pending.pop(future)
                    try:
                        status = future.result()
                    except RemoteStop as exc:
                        stopped = stopped or exc
                        if hasattr(archive.client, "cancel"):
                            archive.client.cancel()
                        status = "paused"
                    yield ref, status
                fill()
        except BaseException:
            if hasattr(archive.client, "cancel"):
                archive.client.cancel()
            for future in pending:
                future.cancel()
            raise
    if stopped:
        raise stopped
