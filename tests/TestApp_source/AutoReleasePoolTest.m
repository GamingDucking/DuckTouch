/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#include "system_headers.h"
static int dealloc_counter = 0;
@implementation DeallocDetection : NSObject
- (void)dealloc {
  dealloc_counter += 1;
}
@end

// Call the runtime entry points explicitly: this file is compiled without
// ARC, so these tests do not depend on compiler return-value optimisations.
extern id objc_retainAutoreleasedReturnValue(id);
extern id objc_autoreleaseReturnValue(id);
extern id objc_retainAutoreleaseReturnValue(id);
extern void *objc_autoreleasePoolPush(void);
extern void objc_autoreleasePoolPop(void *);

static int test_ARCReturnValues(void) {
  dealloc_counter = 0;
  void *outer = objc_autoreleasePoolPush();
  if (!outer) return -10;
  if (objc_retainAutoreleasedReturnValue(nil) != nil ||
      objc_autoreleaseReturnValue(nil) != nil ||
      objc_retainAutoreleaseReturnValue(nil) != nil) return -11;

  DeallocDetection *owned = [DeallocDetection new];
  if (objc_autoreleaseReturnValue(owned) != owned) return -12;
  if (objc_retainAutoreleasedReturnValue(owned) != owned) return -13;
  if ([owned retainCount] != 2) return -14;

  void *inner = objc_autoreleasePoolPush();
  DeallocDetection *temporary = [DeallocDetection new];
  if (objc_retainAutoreleaseReturnValue(temporary) != temporary) return -15;
  if ([temporary retainCount] != 2) return -16;
  [temporary release];
  objc_autoreleasePoolPop(inner);
  if (dealloc_counter != 1 || [owned retainCount] != 2) return -17;

  objc_autoreleasePoolPop(outer);
  if (dealloc_counter != 1 || [owned retainCount] != 1) return -18;
  [owned release];
  if (dealloc_counter != 2) return -19;
  return 0;
}

int test_AutoreleasePool(void) {
  int arc_result = test_ARCReturnValues();
  if (arc_result != 0) return arc_result;
  // Basic test
  {
    dealloc_counter = 0;
    NSAutoreleasePool *arp1 = [NSAutoreleasePool new];
    DeallocDetection *obj1 = [[DeallocDetection new] autorelease];
    DeallocDetection *obj2 = [[[DeallocDetection new] autorelease] retain];
    [arp1 drain];
    if (dealloc_counter != 1) {
      return -1;
    }
    if ([obj2 retainCount] != 1) {
      return -2;
    }
  }

  // Check typical autoreleasepool stack usage
  {
    dealloc_counter = 0;
    // Should not be added to autoreleasepool
    DeallocDetection *obj0 = [DeallocDetection new];
    NSAutoreleasePool *arp1 = [NSAutoreleasePool new];
    DeallocDetection *obj1 = [[DeallocDetection new] autorelease];
    NSAutoreleasePool *arp2 = [NSAutoreleasePool new];
    DeallocDetection *obj2 = [[DeallocDetection new] autorelease];
    NSAutoreleasePool *arp3 = [NSAutoreleasePool new];
    DeallocDetection *obj3 = [[DeallocDetection new] autorelease];
    [arp3 drain];
    if (dealloc_counter != 1 || [obj0 retainCount] != 1 ||
        [obj1 retainCount] != 1 || [obj2 retainCount] != 1) {
      return -3;
    }
    [arp2 drain];
    if (dealloc_counter != 2 || [obj0 retainCount] != 1 ||
        [obj1 retainCount] != 1) {
      return -4;
    }
    [arp1 drain];
    if (dealloc_counter != 3 || [obj0 retainCount] != 1) {
      return -5;
    }
    [obj0 release];
  }

  // Check atypical
  {
    dealloc_counter = 0;
    // Should not be added to autoreleasepool
    NSAutoreleasePool *arp1 = [NSAutoreleasePool new];
    DeallocDetection *obj1 = [[DeallocDetection new] autorelease];
    NSAutoreleasePool *arp2 = [NSAutoreleasePool new];
    DeallocDetection *obj2 = [[DeallocDetection new] autorelease];
    NSAutoreleasePool *arp3 = [NSAutoreleasePool new];
    DeallocDetection *obj3 = [[DeallocDetection new] autorelease];
    [arp2 drain];
    // Should dealloc both arp3 and arp2
    if (dealloc_counter != 2) {
      return -6;
    }
    // Should not dealloc arp1
    if ([obj1 retainCount] != 1) {
      return -7;
    }
    [arp1 drain];
  }
  return 0;
}
