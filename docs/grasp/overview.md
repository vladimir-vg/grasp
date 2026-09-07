# grasp — Overview

Not yet designed. This file is a placeholder.

What is settled is only the shape of the thing: grasp is a Datalog dialect, and
it compiles to [grasp-dbsp](../grasp-dbsp/language.md). It has no runtime of its
own — `grasp-compiler` emits grasp-dbsp, and `grasp-dbsp-runner` executes that.
So [`../grasp-dbsp/language.md`](../grasp-dbsp/language.md) is grasp's output
contract, and the design principles in
[`../grasp-dbsp/overview.md`](../grasp-dbsp/overview.md) describe the target it
emits into rather than anything about grasp itself.
