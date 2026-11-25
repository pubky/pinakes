# Hugging Face

Pinakes reads models, datasets and Spaces on the Hugging Face Hub from the Hub API, by repo ID. No
key is needed.

| | |
| --- | --- |
| Source | `huggingface`, official source |
| Links | `huggingface.co/{owner}/{name}`, `/datasets/{owner}/{name}`, `/spaces/{owner}/{name}` and their API URLs, also on `hf.co`; roots `hf:model/…`, `hf:dataset/…`, `hf:space/…` |
| Fetches | `GET https://huggingface.co/api/{models\|datasets\|spaces}/{owner}/{name}` |
| Credentials | None |
| Rate | At most 1.6 requests per second on `huggingface.co`; the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareSourceCode` (a model), `Dataset`, `WebApplication` (a Space) |
| Keys | `doi:…`, only for DOIs the Hub minted for the repo (`10.57967/hf/…`) |
| Credit | None |
| Switch | `[sources.huggingface] enabled`, on by default |

## Links

Every address of one repo is one resource. The root is `hf:model/{owner}/{name}`,
`hf:dataset/{owner}/{name}` or `hf:space/{owner}/{name}`, in lower case, because the Hub ignores
case in repo IDs. These links are [aliases](../api.md#aliases) of it, on `huggingface.co` and
`hf.co` alike:

- the repo's page: `https://huggingface.co/{owner}/{name}` (a model),
  `https://huggingface.co/datasets/{owner}/{name}` or `https://huggingface.co/spaces/{owner}/{name}`;
- the repo's own tabs after its page: `/tree/main`, `/commits/main`, `/discussions` and `/viewer`
  (the dataset viewer);
- the API URL, `https://huggingface.co/api/{models|datasets|spaces}/{owner}/{name}`.

An owner and a name each hold letters, digits, `-`, `_` and `.`. Neither starts or ends with `-` or
`.`, or holds `--` or `..`. A name doesn't end in `.git`, and the repo ID is at most 96 characters.
An `hf:` root that breaks these rules, names another kind, or is not in lower case is `missing` at
once and never fetched.

When the Hub's API redirects a former name of a renamed repo to its current name, the card's root
is the current repo, and the links by the former name become its aliases. A link in another letter
case has the same root; the API redirects it to the repo's own letter case, which the card's
`name` shows.

Not read here:

- a file (`/blob/…`, `/resolve/…`, `/raw/…`), another revision or a folder (`/tree/{other}`), one
  commit, one discussion and a dataset split: each is another object, not the repo;
- site sections in the place of an owner, in any letter case: `/docs/…`, `/blog/…`, `/papers/…`,
  `/learn/…`, `/organizations/…`, `/settings/…`, `/tasks/…`, `/collections/…`, `/posts/…` and
  others;
- a name without an owner, such as `huggingface.co/gpt2` or `huggingface.co/datasets/gsm8k`. It is
  either a legacy repo ID or a user's or organization's profile, and the link can't tell them apart,
  so these links get ordinary web cards;
- a Space's running app on `*.hf.space`.

## What it fetches

One request per read: `GET https://huggingface.co/api/{models|datasets|spaces}/{owner}/{name}`.
Pinakes follows the Hub's redirects, and the API URL it ends at names the repo.

The Hub answers a request without a token with 401 both for a repo that doesn't exist and for a
private one. Both answers, and a 404, make the card `missing`: there is no public repo. An answer
for another repo is an error, and the read is retried.

## Card

The type is `SoftwareSourceCode` for a model, `Dataset` for a dataset and `WebApplication` for a
Space.

- `site` is "Hugging Face" and `name` the repo ID, in the Hub's own letter case.
- `description` depends on the kind. A dataset's is the text the Hub extracts from its dataset
  card, on one line, without the closing pointer to the full card. A Space's is its short
  description. A model has none.
- The `page_url` fact is the repo's page on huggingface.co. Because the `hf:` root is not a web
  link, the API also lists that page in `open_in`.
- The card has no image and no `attribution`.
- The creation time is a fact (`created`), not a `start`.

| Fact | Meaning |
| --- | --- |
| `created` | When the repo was created (RFC 3339) |
| `author` | The owner (a user or an organization) |
| `title` | The repo card's title or pretty name, when it has one |
| `downloads`, `likes` | As the Hub counts them |
| `pipeline_tag`, `library_name` | A model's task and library, for example `text-generation`, `transformers` |
| `license` | The licence ID or IDs the repo names, for example `mit`, `llama2` |
| `gated` | `false`, `auto` or `manual` (access needs approval) |
| `sdk` | A Space's SDK, for example `gradio` |
| `app_url` | A Space's running app |
| `last_modified` | The time of the last commit (RFC 3339) |
| `arxiv` | The arXiv IDs of the papers the repo's tags name (papers it cites, not the repo itself) |
| `paper_doi` | The other DOIs the repo's tags name, in lower case: the papers it cites, and any DOI the README's own tags name |
| `doi` | The DOIs the Hub minted for the repo (`10.57967/hf/…`), never one the README's own tags name |
| `page_url` | The repo's page on huggingface.co |

## Keys and groups

A DOI the Hub minted for the repo (`10.57967/hf/…`) is its key `doi:…` (the
`own_ids` also-on source records it). The repo then joins the
[group](../api.md#groups) of the `doi:` card for that DOI. A group holds at most one DOI
([keys and groups](../design.md#keys-and-groups)).

A Hub DOI that the README's own tags also name counts as typed by the uploader: it goes to
`paper_doi`, not `doi`. A `paper_doi` and an `arxiv` ID are facts only and join nothing.

## Credit

The card credits nothing. Each repo's content is under the licence it names, which `license`
holds.

## Turn it off

```toml
[sources.huggingface]
enabled = false
```

Hugging Face links then get ordinary web cards, and `hf:` roots wait until the source is on again.

## Examples

- `https://huggingface.co/meta-llama/Llama-2-7b-hf` (a gated model)
- `https://huggingface.co/datasets/openai/gsm8k` (a dataset)
- `https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat` (a Space)
