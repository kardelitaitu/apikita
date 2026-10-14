// Shared model data for the landing page (`/`) and the models page
// (`/models`). Both pages import from here so their figures cannot drift.
//
// Figures are transcribed from config/apikita.toml (the single source of truth)
// as IDR per 1M customer prices, which already include the per-model
// multiplier. Nothing here is estimated. This is transcription with a named
// source on purpose: the website does not read config/ at build time and adds
// no TOML dependency.
//
// **AND THE SEPARATION IS ALSO A FILTER, which is the half of the decision that
// matters and was recorded as a build constraint only.** `config/apikita.toml` describes
// each model in prose - "1M context, vision capable" - and every capability in that
// sentence is RECORDED, NOT ENFORCED: `max_context_tokens`, `supports_vision` and
// `supports_thinking` are named nowhere in the code, and a vision request is not
// refused because the model is marked `supports_vision = true`. If this module read
// the config, a customer model card would inherit a context ceiling and a capability
// the server never checks.
//
// So the hand-written card is a FILTER, not duplication to be automated away. The two
// kinds of copy fail differently and one of them badly: a transcribed PRICE is a stale
// number, corrected by the arithmetic below; a transcribed CAPABILITY is a promise the
// product does not keep, and no amount of accuracy in the transcription prevents it.
// Nothing customer-facing here should be sourced from a key the code does not read.
//
// **THE ARITHMETIC, so a reader can check this by eye in ten seconds** rather than
// trust it, and so a drift is visible rather than silent. Each figure is the config's
// `[models.rates]` PEAK value times the model's `price` multiplier (M = 1.50):
//
//     input  2676.78 x 1.5 =  4015.17  -> 4,015
//     cache    53.54 x 1.5 =    80.31  ->     80
//     output 10707.12 x 1.5 = 16060.68  -> 16,061
//
// and the off-peak column is the same three off the offpeak values:
//     1338.39 -> 2,008 |  26.77 -> 40 |  5353.56 -> 8,030
//
// **NOTHING ENFORCES THIS, and the config header is explicit that it will move.**
// `config/apikita.toml` warns at the top that we collect IDR and pay CNY, so an FX move
// changes our cost with no change in the upstream price list - which is the exact moment
// these six figures go stale, and they are what a customer reads before buying.
//
// The website deliberately does not check it HERE, and that is one decision about the
// SHIPPED SITE: this file adds no TOML dependency, so the config is not read into
// anything the browser downloads. The arithmetic above is the mitigation for that.
//
// **IT IS NOW CHECKED ANYWAY, from the test suite, and my reasoning that it could not
// be was wrong in a way worth recording.** The no-TOML rule constrains the BUILD, not
// the tests: `tests/prices.test.ts` reads config/apikita.toml as TEXT - the same
// technique landing-claims.test.ts already uses on the pages, deliberately - and
// derives these six figures from the config's own rates and multiplier. That adds no
// dependency to the site, because nothing it reads is ever bundled.
//
// The distinction that makes it safe: reading a file in a TEST costs the shipped
// artifact nothing, and a constraint about what the artifact contains is not a
// constraint about what the repository may check. Conflating them is how a correct
// restriction becomes a permanent excuse, and the excuse outlives the reason - by the
// time this was checked, the written arithmetic had been load-bearing for a round.
// checkable by eye, and wrong in a way a reader can see in the same ten seconds.
//
// Worth naming for what it is, though: the side that CAN check computed values - the
// Rust test suite, which reads config/apikita.toml freely - does not cover the website
// transcription, because the transcription is not a value it can see. That boundary is
// the honest limit of the design, not an oversight in it.

// The three token classes, in the order every price array below is aligned to.
export const rateClasses = ['Input', 'Cache hit', 'Output'];

export const rates = [
  { label: 'Input tokens', price: '4,015', note: 'Prompt tokens billed at the standard input rate.' },
  { label: 'Cache-read tokens', price: '80', note: 'Cheapest class. Reported separately, never merged with input.' },
  { label: 'Output tokens', price: '16,061', note: 'Completion tokens. The dominant cost of most requests.' },
];

// The models the proxy exposes by name (config/apikita.toml, [[models]]). Only
// the first two are routed today; the rest are registered and shown on the
// ticker below, each marked on its face as not purchasable.
export const models = ['flash', 'deepseek-v4-flash'];

/**
 * The deposit minimums, from config/apikita.toml `[wallet]`.
 *
 * THEY WERE WRITTEN TWICE IN index.astro - once in the "top up the wallet" step and
 * once in the figures list two hundred lines down - and that page already states the
 * rule this broke: its own copy rule shares the PRICES with /models "so the two pages
 * cannot disagree", while the minimums were duplicated inside one file. A figure stated
 * twice is a figure that gets changed once.
 *
 * Transcribed, and transcribed correctly: 50,000 and 10,000 IDR are
 * `min_first_deposit` and `min_topup`. Same status as the rates above - the website
 * does not read config/ at build time, by decision - so the mitigation is that the
 * number now lives in ONE place and names the key it came from.
 */
export const minFirstDepositIdr = 50000;
export const minTopupIdr = 10000;

/**
 * Top-ups per hour, from config/apikita.toml `[limits] topup_per_hour`.
 *
 * It was a local `const` in the wallet page with a comment naming this key - better than a
 * bare literal, but still a transcription, and a transcription is what the two figures above
 * were. The wallet page is the only surface that shows it, so there is nothing to consolidate
 * *with*; putting it here is what makes it CHECKABLE instead of merely documented, because
 * `tests/prices.test.ts` reads the config's `[limits]` and can compare.
 */
export const topupPerHour = 5;

/**
 * Key-metadata cache TTL, from config/apikita.toml `[key_pool]
 * key_metadata_cache_seconds`.
 *
 * The quickstart tells a developer to allow this long for a LOWERED key limit to take
 * effect, which is a real fact they plan around, and it was a bare number in prose. Same
 * shape as the top-up cap and for the same reason: a value spelled in a surface is a copy
 * not yet consolidated, and a comment cannot keep it honest.
 */
export const keyMetadataCacheSeconds = 60;
export const idr = (n: number) => n.toLocaleString('en-US');

const peakPrices = rates.map((rate) => rate.price);
const offPeakPrices = ['2,008', '40', '8,030'];

export type Availability = 'available' | 'coming-soon' | 'placeholder';

export interface TickerModel {
  name: string;
  availability: Availability;
  note: string;
  peak: string[];
  offPeak: string[];
}

// Six models at two bases: twelve distinct cards, each with its own figures.
// Every number below is the configured rate x the configured multiplier, read
// off the [[models]] blocks in config/apikita.toml — the same source as the
// pricing table above. Nothing here is estimated.
//
// availability is what the customer may actually buy:
//   'available'   — routed today; the peak card is billed and priced in accent.
//   'coming-soon' — deepseek-v4-pro: registered, priced, not yet routed, so
//                   neither basis is a rate anyone can be charged.
//   'placeholder' — MOCK models. Registered with no routable endpoint, no real
//                   provider behind them and no verified price. The card says
//                   so on its face, in the badge and in the sub-line, so a
//                   placeholder figure can never be read as a purchasable rate.
export const tickerModels: TickerModel[] = [
  {
    name: 'flash',
    availability: 'available',
    note: 'also served as deepseek-v4-flash',
    peak: peakPrices,
    offPeak: offPeakPrices,
  },
  {
    name: 'deepseek-v4-flash',
    availability: 'available',
    note: 'legacy alias of flash',
    peak: peakPrices,
    offPeak: offPeakPrices,
  },
  {
    name: 'deepseek-v4-pro',
    availability: 'coming-soon',
    note: 'registered, not yet routed',
    peak: ['18,068', '602', '54,205'],
    offPeak: ['9,034', '301', '27,102'],
  },
  {
    name: 'dummy-glm-5.3-flash',
    availability: 'placeholder',
    note: 'mock placeholder, not for sale',
    peak: ['3,212', '64', '12,849'],
    offPeak: ['1,606', '32', '6,424'],
  },
  {
    name: 'dummy-glm-5.2',
    availability: 'placeholder',
    note: 'mock placeholder, not for sale',
    peak: ['9,636', '482', '38,546'],
    offPeak: ['4,818', '241', '19,273'],
  },
  {
    name: 'dummy-qwen-4-max',
    availability: 'placeholder',
    note: 'mock placeholder, not for sale',
    peak: ['24,091', '1,205', '96,364'],
    offPeak: ['12,046', '602', '48,182'],
  },
];

// One badge per card. A model that is not purchasable says so on every card it
// owns — both bases — never "Billed", so no placeholder figure is ever marked
// as a chargeable rate.
export const tickerCards = tickerModels.flatMap((model) => {
  const mark =
    model.availability === 'placeholder'
      ? 'Placeholder'
      : model.availability === 'coming-soon'
        ? 'Coming soon'
        : null;
  return [
    {
      name: model.name,
      basis: 'Peak',
      billed: model.availability === 'available',
      mark: mark ?? 'Billed',
      alias: model.note,
      prices: model.peak,
    },
    {
      name: model.name,
      basis: 'Off-peak',
      billed: false,
      mark: mark ?? 'Not billed',
      alias: model.note,
      prices: model.offPeak,
    },
  ];
});
