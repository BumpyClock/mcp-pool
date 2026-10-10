use super::*;
use std::time::Duration;
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::Security::{
    ACL_SIZE_INFORMATION, AclSizeInformation, GetAce, GetAclInformation,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    SE_DACL_PROTECTED,
};

#[tokio::test]
async fn first_and_later_instances_accept_only_the_current_user() -> io::Result<()> {
    let name = format!(r"\\.\pipe\mcp-pool-local-security-{}", std::process::id());
    let first = create_named_pipe(&name, true)?;
    assert_eq!(
        create_named_pipe(&name, true)
            .err()
            .map(|error| error.kind()),
        Some(io::ErrorKind::PermissionDenied)
    );
    connect_to_instance(&name, first).await?;

    let later = create_named_pipe(&name, false)?;
    connect_to_instance(&name, later).await
}

async fn connect_to_instance(name: &str, server: NamedPipeServer) -> io::Result<()> {
    let connecting = async {
        let mut last_error =
            io::Error::new(io::ErrorKind::NotFound, "named pipe instance was not ready");
        for _ in 0..50 {
            match connect_named_pipe(Path::new(name)) {
                Ok(client) => return Ok(client),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    last_error = error;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    };

    tokio::time::timeout(Duration::from_secs(5), async {
        let (server_connection, client) = tokio::join!(server.connect(), connecting);
        server_connection?;
        let client = client?;
        verify_named_pipe_server(&client)?;
        drop(server);
        drop(client);
        Ok(())
    })
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error.to_string()))?
}

#[test]
fn constructs_protected_owner_only_pipe_descriptor() -> io::Result<()> {
    let security = PipeSecurity::new()?;
    let descriptor = security.descriptor.get();
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    assert_ne!(control & SE_DACL_PROTECTED, 0);

    let mut owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = current_user_sid_string()?;
    let mut token = open_process_token(unsafe { GetCurrentProcess() })?;
    let user_buffer = token_user_information(token.get())?;
    let user_sid = token_user_sid(&user_buffer)?;
    assert_ne!(unsafe { EqualSid(owner, user_sid) }, 0);

    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    assert_ne!(dacl_present, 0);
    assert!(!dacl.is_null());
    let mut acl_information = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_information as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(acl_information.AceCount, 1);
    let mut ace = ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if ace.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an empty pipe DACL ACE",
        ));
    }
    let allowed = unsafe { &*ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>() };
    assert_eq!(allowed.Header.AceType, 0);
    assert_eq!(allowed.Mask, GENERIC_ALL);
    assert_ne!(
        unsafe { EqualSid(user_sid, ptr::addr_of!(allowed.SidStart).cast_mut().cast()) },
        0
    );
    drop(user_buffer);
    token.close()?;
    assert_eq!(
        owner_only_pipe_sddl(&user),
        format!("O:{user}D:P(A;;GA;;;{user})")
    );
    Ok(())
}

#[test]
fn rejects_nonlocal_pipe_names() {
    assert!(validate_pipe_name(r"\\server\pipe\mcp-pool").is_err());
    assert!(validate_pipe_name(r"\\.\pipe\mcp-pool\nested").is_err());
    assert!(validate_pipe_name(r"\\.\pipe\mcp-pool").is_ok());
}
