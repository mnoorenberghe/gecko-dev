/* -*- indent-tabs-mode: nil; js-indent-level: 2 -*- */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//-----------------------------------------------------------------------------
var BUGNUMBER = 384758;
var summary = 'Statement can not follow expression closure with out intervening ;';
var actual = '';
var expect = '';


//-----------------------------------------------------------------------------
test();
//-----------------------------------------------------------------------------

function test()
{
  printBugNumber(BUGNUMBER);
  printStatus (summary);

  expect = 'SyntaxError: missing ; before statement';
  try
  {
    eval('(function() { if(t) function x() foo() bar(); })');
  }
  catch(ex)
  {
    actual = ex + '';
  }

  reportCompare(expect, actual, summary);
}
