# Hugging Face

Models, datasets and Spaces on the Hugging Face Hub, read from the Hub API by repo id. No key;
the Hub allows 500 API requests per 5 minutes (1.6 per second).

## Links

Every address of one repo is one resource, with the root `hf:model/{owner}/{name}`,
`hf:dataset/{owner}/{name}` or `hf:space/{owner}/{name}` in lower case (the Hub ignores case in
repo ids and redirects any other case to the canonical one, which the card shows):

- `https://huggingface.co/{owner}/{name}` (a model), `https://huggingface.co/datasets/{owner}/{name}`,
  `https://huggingface.co/spaces/{owner}/{name}`, and the same on `hf.co`;
- the repo's own tabs: `/tree/main`, `/commits/main`, `/discussions` and the dataset `/viewer`;
- `https://huggingface.co/api/{models|datasets|spaces}/{owner}/{name}`.

A file (`/blob/…`, `/resolve/…`, `/raw/…`), another revision or folder (`/tree/{other}`), one
commit, one discussion or a dataset split is another object, not an alias of the repo, so those
links are not read here.

Site sections that look like `{owner}/{name}` (`/docs/…`, `/blog/…`, `/papers/…`, `/learn/…`,
`/organizations/…`, `/settings/…`, `/tasks/…`, `/collections/…`, `/posts/…` and so on) are not
repos. A name without an owner (`huggingface.co/gpt2`) is either a legacy repo id or a user's or
organisation's profile, which the link can't tell apart, so those links get ordinary web cards.

When the Hub redirects an old name of a renamed repo to its new one, the card's root is the new
repo and the old links become its aliases.

The Hub answers 401 to a request without a token both for a repo that doesn't exist and for a
private one; both are `missing` (there is no public repo).

## What it reads

`GET https://huggingface.co/api/{models|datasets|spaces}/{owner}/{name}`.

## Card

`SoftwareSourceCode` for a model, `Dataset` for a dataset and `WebApplication` for a Space, with
`site` "Hugging Face" and `name` the repo id; its creation time is the `created` fact. A dataset's
`description` is the text the Hub extracts from its dataset card; a Space's is its short
description; a model has none.

| Fact | Meaning |
| --- | --- |
| `created` | When the repo was created (RFC 3339) |
| `author` | The owner (user or organisation) |
| `title` | The card's title or pretty name, when it has one |
| `downloads`, `likes` | As the Hub counts them |
| `pipeline_tag`, `library_name` | A model's task and library, e.g. `text-generation`, `transformers` |
| `license` | The licence id(s) the repo names, e.g. `mit`, `llama2` |
| `gated` | `false`, `auto` or `manual` (access needs approval) |
| `sdk` | A Space's SDK, e.g. `gradio` |
| `app_url` | A Space's running app |
| `last_modified` | The last commit's time |
| `arxiv` | arXiv ids of the papers the repo cites (not the repo itself) |
| `paper_doi` | DOIs of the papers the repo cites |
| `doi` | The DOIs the Hub minted for the repo (`10.57967/hf/…`) |
| `page_url` | The repo's page on huggingface.co |

## Licence

Each repo's content is under the licence it names (`license`); use of the Hub falls under its
terms of service.

## Examples

- `https://huggingface.co/meta-llama/Llama-2-7b-hf` (a gated model)
- `https://huggingface.co/datasets/openai/gsm8k` (a dataset)
- `https://huggingface.co/spaces/huggingface-projects/llama-2-7b-chat` (a Space)
