#!/usr/bin/env python3
import argparse
import json
import os
import random
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path


TMDB_SEARCH_URL = "https://api.themoviedb.org/3/search/movie"
TMDB_IMAGE_BASE = "https://image.tmdb.org/t/p"


def tmdb_search(title, api_key):
    params = urllib.parse.urlencode({"api_key": api_key, "query": title})
    url = f"{TMDB_SEARCH_URL}?{params}"
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read().decode("utf-8"))


def main():
    parser = argparse.ArgumentParser(description="Add poster_url via TMDB.")
    parser.add_argument(
        "--input",
        default="data/movies.json",
        help="Path to input movies JSON",
    )
    parser.add_argument(
        "--output",
        default="data/movies_new.json",
        help="Path to output movies JSON",
    )
    parser.add_argument(
        "--min-delay",
        type=float,
        default=0.3,
        help="Minimum delay between requests (seconds)",
    )
    parser.add_argument(
        "--max-delay",
        type=float,
        default=0.8,
        help="Maximum delay between requests (seconds)",
    )
    parser.add_argument(
        "--image-size",
        default="w500",
        help="TMDB image size (e.g. w185, w342, w500, original)",
    )
    args = parser.parse_args()

    api_key = os.environ.get("TMDB_API_KEY")
    if not api_key:
        print("TMDB_API_KEY is required", file=sys.stderr)
        raise SystemExit(1)

    input_path = Path(args.input)
    output_path = Path(args.output)
    data = json.loads(input_path.read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise SystemExit("Input JSON must be a list of movies")

    total = len(data)
    successes = 0
    failures = 0

    for idx, movie in enumerate(data, start=1):
        title = movie.get("name")
        if not title:
            movie["poster_url"] = None
            print(f"[{idx}/{total}] missing name -> poster_url=None", flush=True)
            continue

        print(f"[{idx}/{total}] searching TMDB for {title}", flush=True)
        try:
            payload = tmdb_search(title, api_key)
        except Exception as exc:
            failures += 1
            movie["poster_url"] = None
            print(f"[{idx}/{total}] error: {exc}", file=sys.stderr, flush=True)
        else:
            results = payload.get("results") or []
            poster_path = results[0].get("poster_path") if results else None
            if poster_path:
                poster_url = f"{TMDB_IMAGE_BASE}/{args.image_size}{poster_path}"
                movie["poster_url"] = poster_url
                successes += 1
                print(f"[{idx}/{total}] poster_url -> {poster_url}", flush=True)
            else:
                movie["poster_url"] = None
                failures += 1
                print(f"[{idx}/{total}] poster_url not found", flush=True)

        delay = random.uniform(args.min_delay, args.max_delay)
        print(f"[{idx}/{total}] sleeping {delay:.2f}s", flush=True)
        time.sleep(delay)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    print(
        f"done: {successes} posters found, {failures} missing/errors, output={output_path}",
        flush=True,
    )


if __name__ == "__main__":
    main()
