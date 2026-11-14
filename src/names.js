/**
 * 注册用的用户名素材。
 *
 * 之前是随机 6 个小写字母（`jwitbit` 这种），一眼就是机器生成的，
 * 真人注册不会长这样。这里用常见英文名组合，60×60 = 3600 种。
 */

export const FIRST_NAMES = [
  "james", "mary", "john", "patricia", "robert", "jennifer", "michael", "linda",
  "william", "elizabeth", "david", "barbara", "richard", "susan", "joseph", "jessica",
  "thomas", "sarah", "charles", "karen", "christopher", "nancy", "daniel", "lisa",
  "matthew", "betty", "anthony", "margaret", "mark", "sandra", "donald", "ashley",
  "steven", "kimberly", "paul", "emily", "andrew", "donna", "joshua", "michelle",
  "kenneth", "carol", "kevin", "amanda", "brian", "dorothy", "george", "melissa",
  "timothy", "deborah", "ronald", "stephanie", "edward", "rebecca", "jason", "sharon",
  "jeffrey", "laura", "ryan", "cynthia",
];

export const LAST_NAMES = [
  "smith", "johnson", "williams", "brown", "jones", "garcia", "miller", "davis",
  "rodriguez", "martinez", "hernandez", "lopez", "gonzalez", "wilson", "anderson", "thomas",
  "taylor", "moore", "jackson", "martin", "lee", "perez", "thompson", "white",
  "harris", "sanchez", "clark", "ramirez", "lewis", "robinson", "walker", "young",
  "allen", "king", "wright", "scott", "torres", "nguyen", "hill", "flores",
  "green", "adams", "nelson", "baker", "hall", "rivera", "campbell", "mitchell",
  "carter", "roberts", "gomez", "phillips", "evans", "turner", "diaz", "parker",
  "cruz", "edwards", "collins", "reyes",
];

function pick(list) {
  return list[(Math.random() * list.length) | 0];
}

/** 生成一个像真人用户名的名称，例如 `davidmiller`、`sarahnguyen`。 */
export function randName() {
  return pick(FIRST_NAMES) + pick(LAST_NAMES);
}
