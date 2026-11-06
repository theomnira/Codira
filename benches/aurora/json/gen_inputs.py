#!/usr/bin/env python3
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
# Date: November 6, 2026
#
# Functionality:
# - Writes the three inputs of the JSON benchmarks into a directory, the
#   same bytes on every run (a seeded generator, no clock, no network):
#   - twitter.json: an array of 20,000 records shaped like a social
#     network's API (nested objects, strings with escapes and non-ASCII
#     text, integers, booleans, nulls), about 17 MB;
#   - canada.json: a GeoJSON feature collection of polygons whose
#     coordinates are 17-digit floats, like the classic `canada.json`,
#     about 12 MB;
#   - deep.json: 4,000 trees of objects and arrays nested 100 deep,
#     about 10 MB.
#
#   python3 benches/aurora/json/gen_inputs.py <directory>  (scripts/bench.py runs it)

import json
import os
import random
import sys

WORDS = (
    "the quick brown fox jumps over lazy dog json parser value number "
    "string array object nested deep fast slow benchmark codira rust serde "
    "café naïve résumé Ünïcödé 東京 Привет 😀 🚀"
).split()

DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]


def sentence(rng, n):
    words = [rng.choice(WORDS) for _ in range(n)]
    text = " ".join(words)
    # Escapes the printer must write back: quotes, backslashes, controls.
    if rng.random() < 0.3:
        text += ' "quoted"'
    if rng.random() < 0.2:
        text += "\nsecond line\tand a tab"
    if rng.random() < 0.1:
        text += " C:\\path\\to"
    return text


def twitter(rng, count):
    statuses = []
    for i in range(count):
        user_id = rng.randrange(10**6, 10**12)
        status = {
            "id": 1000000000000000000 + i * 7919,
            "id_str": str(1000000000000000000 + i * 7919),
            "created_at": "%s %s %02d %02d:%02d:%02d +0000 2026"
            % (
                rng.choice(DAYS),
                rng.choice(MONTHS),
                rng.randrange(1, 29),
                rng.randrange(24),
                rng.randrange(60),
                rng.randrange(60),
            ),
            "text": sentence(rng, rng.randrange(5, 25)),
            "truncated": rng.random() < 0.1,
            "in_reply_to_status_id": None if rng.random() < 0.7 else rng.randrange(10**15),
            "user": {
                "id": user_id,
                "name": sentence(rng, 2),
                "screen_name": "user_%d" % user_id,
                "location": None if rng.random() < 0.4 else rng.choice(WORDS),
                "description": sentence(rng, rng.randrange(0, 15)),
                "followers_count": rng.randrange(0, 10**7),
                "friends_count": rng.randrange(0, 10**4),
                "verified": rng.random() < 0.05,
                "profile_background_color": "%06X" % rng.randrange(1 << 24),
                "lang": rng.choice(["en", "ja", "es", "fr", "ru"]),
            },
            "retweet_count": rng.randrange(0, 5000),
            "favorite_count": rng.randrange(0, 50000),
            "entities": {
                "hashtags": [
                    {"text": rng.choice(WORDS), "indices": [k * 7, k * 7 + 5]}
                    for k in range(rng.randrange(0, 4))
                ],
                "urls": [],
                "user_mentions": [
                    {
                        "screen_name": "user_%d" % rng.randrange(10**6),
                        "id": rng.randrange(10**9),
                        "indices": [0, 12],
                    }
                    for _ in range(rng.randrange(0, 3))
                ],
            },
            "favorited": False,
            "retweeted": False,
            "lang": "en",
            "geo": None,
            "score": round(rng.random() * 100, 3),
        }
        statuses.append(status)
    return {"statuses": statuses, "search_metadata": {"count": count, "query": "json"}}


def canada(rng, polygons, points):
    features = []
    for p in range(polygons):
        lon, lat = -140.0 + rng.random() * 80.0, 42.0 + rng.random() * 40.0
        ring = []
        for _ in range(points):
            lon += (rng.random() - 0.5) * 0.01
            lat += (rng.random() - 0.5) * 0.01
            # 17 significant digits, as `canada.json` has them.
            ring.append([float("%.15f" % lon), float("%.15f" % lat)])
        features.append(
            {
                "type": "Feature",
                "properties": {"name": "Canada", "part": p},
                "geometry": {"type": "Polygon", "coordinates": [ring]},
            }
        )
    return {"type": "FeatureCollection", "features": features}


def deep_tree(rng, depth):
    if depth == 0:
        return rng.choice([rng.randrange(-1000, 1000), sentence(rng, 1), True, None, 0.5])
    if depth % 2 == 0:
        return {"d": depth, "next": deep_tree(rng, depth - 1), "tag": rng.choice(WORDS)}
    return [depth, deep_tree(rng, depth - 1)]


def deep(rng, trees, depth):
    return [deep_tree(rng, depth - 1) for _ in range(trees)]


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: gen_inputs.py <directory>")
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    inputs = {
        "twitter.json": twitter(random.Random(1), 20000),
        "canada.json": canada(random.Random(2), 60, 5000),
        "deep.json": deep(random.Random(3), 4000, 100),
    }
    for name, value in inputs.items():
        text = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        with open(os.path.join(out, name), "w", encoding="utf-8") as f:
            f.write(text)
        print("%s: %d bytes" % (name, len(text.encode("utf-8"))))


if __name__ == "__main__":
    main()
