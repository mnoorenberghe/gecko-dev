/* -*- Mode: C++; tab-width: 8; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* vim: set ts=8 sts=2 et sw=2 tw=80: */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
#ifndef _mozilla_dom_ClientHandleParent_h
#define _mozilla_dom_ClientHandleParent_h

#include "mozilla/dom/PClientHandleParent.h"

namespace mozilla {
namespace dom {

class ClientHandleParent final : public PClientHandleParent
{
  // PClientHandleParent interface
  mozilla::ipc::IPCResult
  RecvTeardown() override;

  void
  ActorDestroy(ActorDestroyReason aReason) override;

  PClientHandleOpParent*
  AllocPClientHandleOpParent(const ClientOpConstructorArgs& aArgs) override;

  bool
  DeallocPClientHandleOpParent(PClientHandleOpParent* aActor) override;

  mozilla::ipc::IPCResult
  RecvPClientHandleOpConstructor(PClientHandleOpParent* aActor,
                                 const ClientOpConstructorArgs& aArgs) override;

public:
  ClientHandleParent();
  ~ClientHandleParent();

  void
  Init(const IPCClientInfo& aClientInfo);
};

} // namespace dom
} // namespace mozilla

#endif // _mozilla_dom_ClientHandleParent_h
