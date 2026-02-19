There are a few misalignments between the notebook and your blog post:

**1. "Asymmetric IIR" has no description (line 608-610)**

This is the approach you actually shipped, and the blog gives it the richest explanation, yet it's the only strategy section with just a bare `# Asymmetric IIR` heading and no explanatory markdown. The blog calls it an "exponential envelope follower with asymmetric attack and release" and explains the audio processing lineage - none of that context exists in the notebook.

**2. The EWMA description is inaccurate (line 439-441)**

The markdown says "They are blended by taking the max of both windows," but the code at lines 542-546 does a weighted blend: 90/10 short/long when short > long, 20/80 short/long otherwise. That's not "taking the max."

**3. No indication of which strategy won**

The notebook presents three strategies side by side with no indication that the Asymmetric IIR is the one used in the real system. The blog makes it clear the others were attempts that led to the final approach. A reader of the notebook alone wouldn't know the progression.

**4. The "hold" concept spans two strategies in a confusing way**

The blog mentions "attack-hold-release envelopes" as a single concept. In the notebook, "Hold & linear decay" is a separate, simpler strategy, while the actual hold behavior in the IIR (the `in_rhythm` check at line 738 where it suppresses release within 2x the polling interval) isn't mentioned at all. This could confuse someone reading both.

Want me to update the notebook markdown to fix these?
