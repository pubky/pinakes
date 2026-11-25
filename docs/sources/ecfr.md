# eCFR

Pinakes reads sections and parts of the US Code of Federal Regulations from the eCFR (Electronic
Code of Federal Regulations) versioner API. No key is needed. The eCFR pages are behind a bot wall,
and Pinakes never reads them.

| | |
| --- | --- |
| Source | `ecfr`, official source |
| Links | `ecfr.gov/current/title-{title}/…/section-{section}` or `…/part-{part}`; the same under `/on/{YYYY-MM-DD}/` |
| Fetches | `GET https://www.ecfr.gov/api/versioner/v1/ancestry/{current or date}/title-{title}.json?section={section}` (or `?part={part}`) |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.ecfr] enabled`, on by default |

## Links

A section is named by its title and section number, a part by its title and part number. Every
eCFR address of one, on `www.ecfr.gov` or `ecfr.gov`, is an [alias](../api.md#aliases) of its
root. The chapter, subchapter, part, subpart and subject-group segments before the section or part
don't matter.

| Object | Root | Example of an alias |
| --- | --- | --- |
| Section | `https://www.ecfr.gov/current/title-{title}/section-{section}` | `…/title-17/chapter-II/part-240/subpart-A/subject-group-…/section-240.10b-5` |
| Part | `https://www.ecfr.gov/current/title-{title}/part-{part}` | `…/title-14/chapter-I/subchapter-C/part-39` |

The text on a date, `https://www.ecfr.gov/on/{YYYY-MM-DD}/title-{title}/section-{section}` (or
`/part-{part}`), is its own resource, not an alias of the current text. A date written another way
(`/on/2020-1-1/`) is an alias of the `YYYY-MM-DD` root.

These rules decide what is read:

- The title is a number from 1 to 50.
- The address must end in the section or the part. Subpart, subject-group and appendix pages are
  not read, nor is a title on its own.
- A section number is the part, a dot, and letters, digits, hyphens, dots and parentheses
  (`240.10b-5`, `1.401(k)-1`), at most 40 characters. A link that escapes the parentheses (`%28`,
  `%29`) is an alias of the root that doesn't.
- A part named beside a section must be the section's own: `part-240/section-240.10b-5` is read,
  `part-241/section-240.10b-5` is not.
- A fragment (`#p-240.10b-5(a)`) is dropped: the link is the root itself.

Not read here: the legacy `/cgi-bin/text-idx` addresses, search pages and other hosts.

## What it fetches

One request per read, the ancestry of the section or part: its own heading and every level above
it, for the current text or for the link's date.

```
GET https://www.ecfr.gov/api/versioner/v1/ancestry/{current or YYYY-MM-DD}/title-{title}.json?section={section}
GET https://www.ecfr.gov/api/versioner/v1/ancestry/{current or YYYY-MM-DD}/title-{title}.json?part={part}
```

A section or part eCFR doesn't know (404) is `missing`. An ancestry that doesn't end at the
section or part asked for is an error. A rate limit (429) is retried after at least the time its
`Retry-After` header names (up to an hour), or 5 minutes when it names none. A timeout (408) or a
server error is retried after at least a minute.

## Card

Every card is `Legislation`.

- `name` is the object's label and heading, for example "§ 240.10b-5 Employment of manipulative
  and deceptive devices." or "Part 39 Airworthiness Directives".
- `description` lists the title, chapter and part above the object, for example "Title 17 —
  Commodity and Securities Exchanges · Chapter II — Securities and Exchange Commission · Part 240 —
  General Rules and Regulations, Securities Exchange Act of 1934".

| Fact | Meaning |
| --- | --- |
| `citation` | For example `17 CFR 240.10b-5` or `14 CFR 39` |
| `cfr_title` | The heading of its title |
| `chapter` | The heading of its chapter |
| `cfr_part` | The heading of its part (on a part's card, its own heading) |
| `reserved` | `true` for a reserved (empty) section or part |
| `version_date` | For the text on a date: that date, `YYYY-MM-DD` |
| `page_url` | Its root eCFR address |

## Keys and groups

None. The text on a date has the same `citation` as the current text, but the citation is a fact
and joins nothing.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.ecfr]
enabled = false
```

eCFR links then get ordinary web cards.

## Examples

- `https://www.ecfr.gov/current/title-17/section-240.10b-5`
- `https://www.ecfr.gov/current/title-14/chapter-I/subchapter-C/part-39`
- `https://www.ecfr.gov/on/2024-01-01/title-17/section-240.10b-5` (the section on a date)
