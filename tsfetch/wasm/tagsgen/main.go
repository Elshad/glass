// Command tagsgen prints the build tags that leave out every Tailscale
// feature tsfetch does not use. build.sh passes them to go build.
package main

import (
	"fmt"
	"strings"

	"tailscale.com/feature/featuretags"
)

// keep lists the features that stay linked in. Everything else in
// featuretags.Features is omitted.
var keep = []featuretags.FeatureTag{
	"bakedroots", // Let's Encrypt roots, for https:// to *.ts.net hosts
	"c2n",        // control-to-node requests
	"dns",        // MagicDNS names
	"health",
	"ipnbus", // state notifications
	"netstack",
}

func main() {
	for _, ft := range keep {
		if _, ok := featuretags.Features[ft]; !ok {
			panic(fmt.Sprintf("unknown feature tag %q", ft))
		}
	}
	fmt.Println(strings.Join(featuretags.MinTags(keep...), ","))
}
