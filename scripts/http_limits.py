"""Bound decoded input read by HTTP diagnostic tools before JSON parsing."""
MAX_RESPONSE_BYTES = 8 * 1024 * 1024


def read_response(stream, *, label='HTTP response') -> bytes:
    data = stream.read(MAX_RESPONSE_BYTES + 1)
    if len(data) > MAX_RESPONSE_BYTES:
        raise RuntimeError(f'{label} exceeds the 8MiB response limit')
    return data
