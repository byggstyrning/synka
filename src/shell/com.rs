//! Class factories for the classes in ui/Package.appxmanifest and their registration
//! (CoRegisterClassObject, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE). Registered on the main
//! STA thread in every mode, so a running engine is what Explorer reaches; COM then never
//! starts a second process for these CLSIDs.

use super::flyout::StatusFactory;
use super::states::CustomStates;
use super::verbs::Verb;
use super::{ShellVerb, CLSID_CUSTOM_STATES, CLSID_STATUS_UI};
use std::ffi::c_void;
use windows::core::{implement, IUnknown, Interface, Ref, BOOL, GUID};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{
    CoRegisterClassObject, CoRevokeClassObject, IClassFactory, IClassFactory_Impl,
    CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE,
};

#[derive(Debug, Clone, Copy)]
enum Class {
    CustomStates,
    StatusUi,
    Verb(ShellVerb),
}

impl Class {
    fn all() -> impl Iterator<Item = Class> {
        [Class::CustomStates, Class::StatusUi]
            .into_iter()
            .chain(ShellVerb::ALL.into_iter().map(Class::Verb))
    }

    fn clsid(self) -> GUID {
        match self {
            Class::CustomStates => CLSID_CUSTOM_STATES,
            Class::StatusUi => CLSID_STATUS_UI,
            Class::Verb(v) => v.clsid(),
        }
    }

    fn create(self) -> IUnknown {
        match self {
            Class::CustomStates => CustomStates.into(),
            Class::StatusUi => StatusFactory.into(),
            Class::Verb(v) => Verb(v).into(),
        }
    }
}

#[implement(IClassFactory)]
struct Factory(Class);

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppv = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let obj = self.0.create();
        unsafe { obj.query(riid, ppv).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        // The engine outlives every Explorer client; nothing to count.
        Ok(())
    }
}

/// Registered class objects; revoked on drop (on the registering thread).
pub struct ClassObjects {
    cookies: Vec<u32>,
}

impl ClassObjects {
    /// Register every class on the calling STA thread. Failures are logged: the engine works
    /// without Explorer integration.
    pub fn register() -> ClassObjects {
        let mut cookies = Vec::new();
        for class in Class::all() {
            let factory: IClassFactory = Factory(class).into();
            match unsafe {
                CoRegisterClassObject(
                    &class.clsid(),
                    &factory,
                    CLSCTX_LOCAL_SERVER,
                    REGCLS_MULTIPLEUSE,
                )
            } {
                Ok(cookie) => cookies.push(cookie),
                Err(e) => {
                    tracing::warn!("CoRegisterClassObject {:?} ({class:?}): {e}", class.clsid())
                }
            }
        }
        tracing::debug!(classes = cookies.len(), "COM-klasser registrerade");
        ClassObjects { cookies }
    }
}

impl Drop for ClassObjects {
    fn drop(&mut self) {
        for cookie in self.cookies.drain(..) {
            unsafe {
                let _ = CoRevokeClassObject(cookie);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HSTRING;
    use windows::Storage::Provider::IStorageProviderItemPropertySource;
    use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
    use windows::Win32::UI::Shell::{IExplorerCommand, IShellItemArray, ECS_HIDDEN};

    #[test]
    fn registers_and_revokes_every_class() {
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let objects = ClassObjects::register();
        assert_eq!(objects.cookies.len(), Class::all().count());
        drop(objects);
        if com {
            unsafe { CoUninitialize() };
        }
    }

    /// Drives every class through its factory and vtable, the way COM does after activation.
    #[test]
    fn factories_hand_out_working_objects() {
        for class in Class::all() {
            let factory: IClassFactory = Factory(class).into();
            let outer: IUnknown = Factory(class).into();
            let aggregated: windows::core::Result<IUnknown> =
                unsafe { factory.CreateInstance(&outer) };
            assert_eq!(aggregated.unwrap_err().code(), CLASS_E_NOAGGREGATION);
            match class {
                Class::CustomStates => {
                    let src: IStorageProviderItemPropertySource =
                        unsafe { factory.CreateInstance(None) }.unwrap();
                    // No engine context in a test: an empty answer, not an error.
                    let props = src
                        .GetItemProperties(&HSTRING::from(r"C:\Synka\Hub\Proj\a.rvt"))
                        .unwrap();
                    assert_eq!(props.into_iter().count(), 0);
                    assert!(
                        unsafe { factory.CreateInstance::<_, IExplorerCommand>(None) }.is_err()
                    );
                }
                Class::StatusUi => {
                    let f: windows::Storage::Provider::IStorageProviderStatusUISourceFactory =
                        unsafe { factory.CreateInstance(None) }.unwrap();
                    let source = f.GetStatusUISource(&HSTRING::from("synka!test")).unwrap();
                    // No engine context in a test: the signed-out flyout, not an error. The
                    // flyout classes exist on Windows 11 only (not on Windows 10, Server or Wine).
                    const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154u32 as i32;
                    match source.GetStatusUI() {
                        Ok(ui) => {
                            assert_eq!(ui.ProviderStateLabel().unwrap().to_string(), "Logga in")
                        }
                        Err(e) => assert_eq!(e.code().0, REGDB_E_CLASSNOTREG, "{e}"),
                    }
                }
                Class::Verb(verb) => {
                    let cmd: IExplorerCommand = unsafe { factory.CreateInstance(None) }.unwrap();
                    unsafe {
                        let title = cmd.GetTitle(None::<&IShellItemArray>).unwrap();
                        assert_eq!(title.to_string().unwrap(), verb.title());
                        CoTaskMemFree(Some(title.0 as *const c_void));
                        assert_eq!(cmd.GetCanonicalName().unwrap(), verb.clsid());
                        assert_eq!(
                            cmd.GetState(None::<&IShellItemArray>, true).unwrap(),
                            ECS_HIDDEN.0 as u32
                        );
                        assert!(
                            cmd.Invoke(None::<&IShellItemArray>, None::<&IBindCtx>)
                                .is_err(),
                            "no selection"
                        );
                    }
                }
            }
        }
    }
}
