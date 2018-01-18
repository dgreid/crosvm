// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Exported interface to basic qcow functionality to be used from C.

// Create a basic, empty qcow2 file that can grow to `virtual_size` at `path`.
int create_qcow_with_size(char *const path, unsigned long virtual_size);
