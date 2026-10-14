#!/usr/bin/env python3
"""Renew selected CPI/NFP and regular FOMC dates from official sources.

This is not an all-event calendar. Unscheduled releases and provider changes
remain external risks. A failed refresh never replaces the last valid file.
"""
from __future__ import annotations

import argparse
import calendar
from datetime import datetime, timedelta, timezone
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import re
import tempfile
from urllib.request import HTTPRedirectHandler, Request, build_opener
from zoneinfo import ZoneInfo

from native_tls_runtime import require_safe_tls

BLS_URL = "https://www.bls.gov/schedule/news_release/bls.ics"
FED_URL = "https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm"
MAX_BYTES = 1024 * 1024
EASTERN = ZoneInfo("America/New_York")
UTC = timezone.utc


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("official calendar redirect refused; review the new source first")


def fetch(url: str) -> bytes:
    if url not in (BLS_URL, FED_URL):
        raise ValueError("unsupported calendar source")
    require_safe_tls(url)
    request = Request(url, headers={"User-Agent": "Macbot calendar research/1.0", "Accept-Encoding": "identity"})
    with build_opener(NoRedirect()).open(request, timeout=10) as response:
        data = response.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError("official calendar exceeds 1MiB")
    return data


def timestamp(value: datetime) -> str:
    return value.astimezone(UTC).isoformat().replace("+00:00", "Z")


def parse_bls(data: bytes) -> list[dict]:
    if len(data) > MAX_BYTES:
        raise ValueError("BLS calendar exceeds 1MiB")
    lines = []
    for line in data.decode("utf-8-sig").splitlines():
        if line.startswith((" ", "\t")):
            if not lines:
                raise ValueError("invalid iCalendar folding")
            lines[-1] += line[1:]
        else:
            lines.append(line)
    if not lines or lines[0] != "BEGIN:VCALENDAR" or lines[-1] != "END:VCALENDAR":
        raise ValueError("BLS did not return a complete iCalendar")
    events = []
    properties = None
    for line in lines:
        if line == "BEGIN:VEVENT":
            if properties is not None:
                raise ValueError("nested calendar event")
            properties = {}
        elif line == "END:VEVENT":
            if properties is None:
                raise ValueError("unmatched calendar event")
            summary = properties.get("SUMMARY", ("", ""))[1]
            kind = "cpi" if "Consumer Price Index" in summary else "nfp" if "Employment Situation" in summary else None
            if kind and properties.get("STATUS", ("", ""))[1] != "CANCELLED":
                if any(key in properties for key in ("RRULE", "RDATE", "EXDATE", "RECURRENCE-ID")):
                    raise ValueError("selected BLS event has unsupported recurrence")
                if "DTSTART" not in properties:
                    raise ValueError("selected BLS release has no timestamp")
                name, raw = properties["DTSTART"]
                if raw.endswith("Z"):
                    when = datetime.strptime(raw, "%Y%m%dT%H%M%SZ").replace(tzinfo=UTC)
                else:
                    match = re.fullmatch(r'DTSTART;TZID="?([^";]+)"?', name)
                    if not match or match[1] not in ("America/New_York", "US/Eastern"):
                        raise ValueError("selected BLS release lacks a supported explicit timezone")
                    when = datetime.strptime(raw, "%Y%m%dT%H%M%S").replace(tzinfo=EASTERN)
                events.append({"kind": kind, "label": summary, "timestamp": timestamp(when), "source": BLS_URL})
            properties = None
        elif properties is not None:
            if ":" not in line:
                raise ValueError("invalid calendar property")
            name, value = line.split(":", 1)
            key = name.split(";", 1)[0]
            if key in properties:
                raise ValueError("duplicate calendar property")
            properties[key] = (name, value)
    if properties is not None:
        raise ValueError("truncated calendar event")
    return events


class FedCalendarParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack = []
        self.heading = None
        self.year = None
        self.row = None
        self.capture = None
        self.meetings = []

    def handle_starttag(self, tag, attrs):
        if tag in {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}:
            return
        self.stack.append(tag)
        depth = len(self.stack)
        classes = dict(attrs).get("class", "").split()
        if tag == "h4":
            self.heading = (depth, [])
        if "fomc-meeting" in classes:
            if self.row is not None:
                raise ValueError("nested FOMC row")
            self.row = {"depth": depth, "year": self.year, "month": [], "date": []}
        if self.row is not None:
            for field in ("month", "date"):
                if f"fomc-meeting__{field}" in classes:
                    self.capture = (depth, field)

    def handle_data(self, data):
        if self.heading is not None:
            self.heading[1].append(data)
        if self.row is not None and self.capture is not None:
            self.row[self.capture[1]].append(data)

    def handle_endtag(self, tag):
        if tag not in self.stack:
            return
        depth = len(self.stack) - self.stack[::-1].index(tag)
        if self.heading is not None and self.heading[0] >= depth:
            match = re.fullmatch(r"\s*(\d{4}) FOMC Meetings\s*", "".join(self.heading[1]))
            if match:
                self.year = int(match[1])
            self.heading = None
        if self.capture is not None and self.capture[0] >= depth:
            self.capture = None
        if self.row is not None and self.row["depth"] >= depth:
            self.meetings.append(self.row)
            self.row = None
        del self.stack[depth - 1:]


def parse_fed(data: bytes) -> list[dict]:
    if len(data) > MAX_BYTES:
        raise ValueError("Fed calendar exceeds 1MiB")
    parser = FedCalendarParser()
    parser.feed(data.decode("utf-8"))
    if parser.row is not None:
        raise ValueError("truncated FOMC row")
    events = []
    months = {name.lower(): i for i, name in enumerate(calendar.month_name) if name}
    months.update({name.lower(): i for i, name in enumerate(calendar.month_abbr) if name})
    for row in parser.meetings:
        month = "".join(row["month"]).strip().split("/")[-1].lower()
        date = "".join(row["date"]).strip().replace("*", "")
        if re.fullmatch(r"\d{1,2} \(notation vote\)", date):
            continue  # Explicitly outside the regular-statement scope.
        match = re.fullmatch(r"(\d{1,2})(?:[-–](\d{1,2}))?", date)
        if row["year"] is None or month not in months or not match:
            raise ValueError("unrecognized official FOMC meeting row")
        # Regular statements are scheduled at 14:00 Eastern on the final day.
        # https://www.federalreserve.gov/newsevents/pressreleases/monetary20130313a.htm
        when = datetime(row["year"], months[month], int(match[2] or match[1]), 14, tzinfo=EASTERN)
        events.append({"kind": "fomc", "label": "FOMC Statement (regular schedule)", "timestamp": timestamp(when), "source": FED_URL})
    if not events:
        raise ValueError("official FOMC meeting rows missing")
    return events


def make_document(bls: bytes, fed: bytes, now: datetime) -> dict:
    if now.tzinfo is None:
        raise ValueError("refresh time must be timezone-aware")
    horizon = now + timedelta(days=400)
    events = [event for event in parse_bls(bls) + parse_fed(fed)
              if now - timedelta(hours=4) <= datetime.fromisoformat(event["timestamp"]) <= horizon]
    identities = {(event["kind"], event["timestamp"]) for event in events}
    if len(identities) != len(events) or len(events) > 4096:
        raise ValueError("duplicate/excess official calendar events")
    for kind in ("cpi", "nfp", "fomc"):
        if not any(event["kind"] == kind and datetime.fromisoformat(event["timestamp"]) > now for event in events):
            raise ValueError(f"official calendar has no future {kind} coverage")
    return {"schema_version": 1, "refreshed_at": timestamp(now),
            "valid_until": timestamp(now + timedelta(hours=36)),
            "sources": [{"url": url, "sha256": hashlib.sha256(data).hexdigest()}
                        for url, data in ((BLS_URL, bls), (FED_URL, fed))],
            "scope": "CPI, Employment Situation and regular FOMC statements only; no unscheduled events",
            "events": sorted(events, key=lambda event: event["timestamp"])}


def write_atomic(path: Path, document: dict) -> None:
    if path.is_symlink():
        raise ValueError("calendar output must not be a symlink")
    data = (json.dumps(document, indent=2) + "\n").encode()
    if len(data) > MAX_BYTES:
        raise ValueError("generated calendar exceeds 1MiB")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".macro-calendar-", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        document = make_document(fetch(BLS_URL), fetch(FED_URL), datetime.now(UTC))
        write_atomic(args.output, document)
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"Calendar refresh refused: {error}\n")
    print(f"Renewed {len(document['events'])} selected official events; expires {document['valid_until']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
