======================================================================
Relative-position: A→B
======================================================================

Delay (ms) [- = timed out]
Method sync a1000 b1000 a2000 b2000

---

switch 429 - 35 - 23
sub-update-forward 569 1567 568 2565 565
joining-fetch 571 1568 570 3566 564

Stall (ms) [J=500ms]
Method sync a1000 b1000 a2000 b2000

---

switch 0 - 0 - 0
sub-update-forward 0 961 0 1960 0
joining-fetch 0 961 0 1964 0

Skipped (ms)
Method sync a1000 b1000 a2000 b2000

---

switch 0 - 0 - 0
sub-update-forward 0 0 0 0 1000
joining-fetch 0 0 0 0 1000

AETR
Method sync a1000 b1000 a2000 b2000

---

switch 0.0 - 0.2753 - 0.2827
sub-update-forward 0.4762 1.4881 0.4762 2.4762 0.4643
joining-fetch 0.0 1.0 0.0 2.0 0.0

======================================================================
Relative-position: B→A
======================================================================

Delay (ms) [- = timed out]
Method sync a1000 b1000 a2000 b2000

---

switch 567 32 2570 32 -
sub-update-forward 570 567 1563 563 2568
joining-fetch 8 9 1568 9 3565

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
sub-update-forward 0 1000 0 2000 0
joining-fetch 0 0 0 0 0

AETR
Method sync a1000 b1000 a2000 b2000

---

switch 0.0 0.7047 1.0 0.7047 -
sub-update-forward 0.7048 0.7048 1.4643 0.6929 2.4881
joining-fetch 0.0 0.0 1.0 0.0 2.0

======================================================================
Bandwidth conditions
======================================================================

Delay (ms) [- = timed out]
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 667 400 3895 353
sub-update-forward 602 441 7004 366
joining-fetch 4560 1627 4475 473

Stall (ms) [J=500ms]
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 49 31 3418 0
sub-update-forward 176 39 4551 0
joining-fetch 2129 547 3901 315

Skipped (ms)
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 0 0 2000 0
sub-update-forward 0 0 0 0
joining-fetch 0 0 0 0

AETR
Method a2b_4500k a2b_7000k b2a_3000k b2a_7000k

---

switch 0.0 0.0 2.0 0.0
sub-update-forward 0.4063 0.317 2.5357 0.5357
joining-fetch 4.4444 1.0 3.2013 0.7619

======================================================================
Downstream delay: A→B
======================================================================

Delay (ms) [- = timed out]
Method 0ms 500ms

---

switch 568 1344
sub-update-forward 566 848
joining-fetch 571 1376

Stall (ms) [J=500ms]
Method 0ms 500ms

---

switch 1 0
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

switch 0.0 0.0
sub-update-forward 0.4762 0.5402
joining-fetch 0.0 0.2902

======================================================================
Downstream delay: B→A
======================================================================

Delay (ms) [- = timed out]
Method 0ms 500ms

---

switch 566 1319
sub-update-forward 565 836
joining-fetch 7 1347

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

switch 0.0 0.0
sub-update-forward 0.7048 1.2071
joining-fetch 0.0 1.9857
