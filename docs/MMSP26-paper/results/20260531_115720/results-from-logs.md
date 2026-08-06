======================================================================
Relative-position: A→B
======================================================================

Delay (ms) [- = timed out]
Method sync a1000 b1000 a2000 b2000

---

switch 430 - 36 - 23
sub-update-forward 570 1568 569 2565 565
joining-fetch 572 1569 571 3567 564

Stall (ms) [J=500ms]
Method sync a1000 b1000 a2000 b2000

---

switch 0 - 0 - 0
sub-update-forward 0 961 0 1960 0
joining-fetch 560 961 547 1964 520

Skipped (ms)
Method sync a1000 b1000 a2000 b2000

---

switch 0 - 0 - 0
sub-update-forward 0 0 0 0 1000
joining-fetch 0 0 1000 0 2000

AETR
Method sync a1000 b1000 a2000 b2000

---

switch 0.0 - 0.2991 - 0.2947
sub-update-forward 0.506 1.5357 0.506 2.4762 0.5714
joining-fetch 0.7896 1.0 2.0015 2.0 2.9628

======================================================================
Relative-position: B→A
======================================================================

Delay (ms) [- = timed out]
Method sync a1000 b1000 a2000 b2000

---

switch 567 33 2571 32 -
sub-update-forward 571 567 1563 564 2569
joining-fetch 8 9 1568 10 3566

Stall (ms) [J=500ms]
Method sync a1000 b1000 a2000 b2000

---

switch 0 0 1998 0 -
sub-update-forward 4 1 957 0 1957
joining-fetch 0 0 958 0 1958

Skipped (ms)
Method sync a1000 b1000 a2000 b2000

---

switch 0 0 1000 0 -
sub-update-forward 0 667 0 1667 0
joining-fetch 0 0 0 0 0

AETR
Method sync a1000 b1000 a2000 b2000

---

switch 0.0119 0.7166 1.0 0.7166 -
sub-update-forward 0.7048 0.7524 1.5119 0.7405 2.4881
joining-fetch 0.0 0.0 1.0 0.0 2.0

======================================================================
Bandwidth conditions
======================================================================

Delay (ms) [- = timed out]
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 667 400 3896 353
sub-update-forward 602 441 7005 367
joining-fetch 4561 1628 4475 473

Stall (ms) [J=500ms]
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 49 31 3418 0
sub-update-forward 176 39 4551 0
joining-fetch 4463 1547 3901 315

Skipped (ms)
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 0 0 2000 0
sub-update-forward 0 0 0 0
joining-fetch 2333 1000 0 0

AETR
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 0.0 0.0 2.0 0.0
sub-update-forward 0.4063 0.317 2.5476 0.5595
joining-fetch 5.7051 2.375 2.1905 0.7619

======================================================================
Downstream delay: A→B
======================================================================

Delay (ms) [- = timed out]
Method 0ms 500ms

---

switch 569 1344
sub-update-forward 567 849
joining-fetch 571 1376

Stall (ms) [J=500ms]
Method 0ms 500ms

---

switch 1 0
sub-update-forward 0 0
joining-fetch 538 796

Skipped (ms)
Method 0ms 500ms

---

switch 0 0
sub-update-forward 0 0
joining-fetch 0 0

AETR
Method 0ms 500ms

---

switch 0.0238 0.0
sub-update-forward 0.4762 0.5521
joining-fetch 0.7541 1.5618

======================================================================
Downstream delay: B→A
======================================================================

Delay (ms) [- = timed out]
Method 0ms 500ms

---

switch 567 1319
sub-update-forward 566 836
joining-fetch 8 1348

Stall (ms) [J=500ms]
Method 0ms 500ms

---

switch 0 0
sub-update-forward 0 0
joining-fetch 0 0

Skipped (ms)
Method 0ms 500ms

---

switch 0 0
sub-update-forward 0 0
joining-fetch 0 0

AETR
Method 0ms 500ms

---

switch 0.0119 0.0119
sub-update-forward 0.7524 1.2071
joining-fetch 0.0 1.9857
