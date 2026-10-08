#!/usr/bin/env bash
# Download the recognizer's training data into <data-dir>:
#
#   <data-dir>/google-fonts  open-licensed fonts (SIL OFL / Apache 2.0) from
#                            github.com/google/fonts, sparse-checked-out
#   <data-dir>/corpus        public-domain books from Project Gutenberg
#
# Nothing here is committed to the repository; the trained weights are.
# Usage: tools/ocr_train/prepare_data.sh <data-dir>

set -euo pipefail
DATA="${1:?usage: prepare_data.sh <data-dir>}"
mkdir -p "$DATA/corpus"

# Serif, sans, monospace, condensed and typewriter faces, including
# metric-compatible stand-ins for the fonts documents use most: Arimo (Arial),
# Tinos (Times New Roman), Cousine (Courier New), Carlito (Calibri), Caladea
# (Cambria) and Gelasio (Georgia). Keep the evaluation fonts out of this list.
FAMILIES=(
  ofl/arimo ofl/tinos ofl/cousine ofl/carlito ofl/caladea ofl/gelasio
  ofl/ptserif ofl/sourceserif4 ofl/merriweather ofl/lora ofl/ebgaramond ofl/librebaskerville
  ofl/crimsontext ofl/notoserif apache/robotoslab ofl/zillaslab ofl/oldstandardtt ofl/spectral
  ofl/literata ofl/ibmplexserif ofl/opensans ofl/lato ofl/notosans ofl/sourcesans3 ofl/ptsans
  ofl/firasans ofl/worksans ofl/montserrat ofl/raleway ofl/nunito ofl/ibmplexsans ofl/inter
  ofl/archivonarrow ofl/robotocondensed ofl/oswald ofl/barlow ofl/publicsans ofl/overpass
  ofl/courierprime ofl/robotomono ofl/sourcecodepro ofl/ibmplexmono ofl/inconsolata ofl/spacemono
  ofl/jetbrainsmono apache/specialelite ofl/cutivemono ofl/roboto ofl/notosansmono ofl/karla
  ofl/mulish ofl/cardo ofl/vollkorn ofl/alegreya ofl/alegreyasans ofl/dmsans ofl/asap ofl/cabin
  ofl/questrial ofl/hind ofl/josefinsans ofl/quattrocento ofl/arvo
)
if [ ! -d "$DATA/google-fonts" ]; then
  git clone --quiet --depth 1 --filter=blob:none --sparse https://github.com/google/fonts "$DATA/google-fonts"
fi
patterns=()
for family in "${FAMILIES[@]}"; do patterns+=("/$family/*"); done
git -C "$DATA/google-fonts" sparse-checkout set --no-cone "${patterns[@]}"
echo "fonts: $(find "$DATA/google-fonts" -name '*.ttf' | wc -l | tr -d ' ') files"

# English prose of several periods and registers, plus French, German,
# Spanish, Italian and Portuguese for their accents.
BOOKS=(1342 84 1661 98 345 2701 174 43 3300 1497 5200 2554 76 36 1232 20203 2591 1260 768 205
       1400 10 5 829 4300 1080 17489 4650 5097 17989 2229 22367 2000 1012 3333 6797 7849 15725 25530)
for id in "${BOOKS[@]}"; do
  out="$DATA/corpus/pg$id.txt"
  [ -s "$out" ] || curl -sfL -o "$out" "https://www.gutenberg.org/cache/epub/$id/pg$id.txt" || echo "failed: $id"
  sleep 0.3
done
echo "corpus: $(ls "$DATA/corpus" | wc -l | tr -d ' ') books"
