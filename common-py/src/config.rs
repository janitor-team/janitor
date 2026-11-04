use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;

#[pyclass]
pub struct Config(pub(crate) janitor::config::Config);

#[pymethods]
impl Config {
    pub fn find_distribution(&self, name: &str) -> PyResult<Distribution> {
        let d = self
            .0
            .distribution
            .iter()
            .find(|d| d.name.as_ref().unwrap() == name)
            .map(|d| Distribution(d.clone()));

        if let Some(d) = d {
            Ok(d)
        } else {
            Err(PyKeyError::new_err(format!(
                "No distribution named '{}'",
                name
            )))
        }
    }

    pub fn find_campaign(&self, name: &str) -> PyResult<Campaign> {
        let c = self
            .0
            .campaign
            .iter()
            .find(|c| c.name.as_ref().unwrap() == name)
            .map(|c| Campaign(c.clone()));
        if let Some(c) = c {
            Ok(c)
        } else {
            Err(PyKeyError::new_err(format!("No campaign named '{}'", name)))
        }
    }

    #[getter]
    pub fn logs_location(&self) -> Option<&str> {
        self.0.logs_location.as_deref()
    }

    #[getter]
    pub fn database_location(&self) -> Option<&str> {
        self.0.database_location.as_deref()
    }

    #[getter]
    pub fn committer(&self) -> Option<&str> {
        self.0.committer.as_deref()
    }

    #[getter]
    pub fn distribution(&self) -> Vec<Distribution> {
        self.0
            .distribution
            .iter()
            .map(|d| Distribution(d.clone()))
            .collect()
    }

    #[getter]
    pub fn origin(&self) -> Option<&str> {
        self.0.origin.as_deref()
    }

    #[getter]
    pub fn artifact_location(&self) -> Option<&str> {
        self.0.artifact_location.as_deref()
    }

    #[getter]
    pub fn oauth2_provider(&self) -> Option<OAuth2Provider> {
        self.0
            .oauth2_provider
            .as_ref()
            .map(|p| OAuth2Provider(p.clone()))
    }

    #[getter]
    pub fn zipkin_address(&self) -> Option<&str> {
        self.0.zipkin_address.as_deref()
    }

    #[getter]
    pub fn campaign(&self) -> Vec<Campaign> {
        self.0
            .campaign
            .iter()
            .map(|c| Campaign(c.clone()))
            .collect()
    }

    #[getter]
    pub fn git_location(&self) -> Option<&str> {
        self.0.git_location.as_deref()
    }

    #[getter]
    pub fn bzr_location(&self) -> Option<&str> {
        self.0.bzr_location.as_deref()
    }

    #[getter]
    pub fn redis_location(&self) -> Option<&str> {
        self.0.redis_location.as_deref()
    }

    #[getter]
    pub fn user_agent(&self) -> Option<&str> {
        self.0.user_agent.as_deref()
    }

    #[getter]
    pub fn bugtracker(&self) -> Vec<BugTracker> {
        self.0
            .bugtracker
            .iter()
            .map(|b| BugTracker(b.clone()))
            .collect()
    }

    #[getter]
    pub fn apt_repository(&self) -> Vec<AptRepository> {
        self.0
            .apt_repository
            .iter()
            .map(|a| AptRepository(a.clone()))
            .collect()
    }

    #[getter]
    pub fn hide_reviewer_identities(&self) -> bool {
        self.0.hide_reviewer_identities.unwrap_or(false)
    }
}

#[pyclass]
pub struct BugTracker(pub(crate) janitor::config::BugTracker);

#[pymethods]
impl BugTracker {
    #[getter]
    pub fn url(&self) -> Option<&str> {
        self.0.url.as_deref()
    }

    #[getter]
    pub fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    #[getter]
    pub fn kind(&self) -> PyResult<&str> {
        match self.0.kind.unwrap().unwrap() {
            janitor::config::BugTrackerKind::debian => Ok("debian"),
            janitor::config::BugTrackerKind::github => Ok("github"),
            janitor::config::BugTrackerKind::gitlab => Ok("gitlab"),
        }
    }
}

#[pyclass]
pub struct OAuth2Provider(pub(crate) janitor::config::OAuth2Provider);

#[pymethods]
impl OAuth2Provider {
    #[getter]
    pub fn client_id(&self) -> Option<&str> {
        self.0.client_id.as_deref()
    }

    #[getter]
    pub fn client_secret(&self) -> Option<&str> {
        self.0.client_secret.as_deref()
    }

    #[getter]
    pub fn base_url(&self) -> Option<&str> {
        self.0.base_url.as_deref()
    }

    #[getter]
    pub fn qa_reviewer_group(&self) -> Option<&str> {
        self.0.qa_reviewer_group.as_deref()
    }

    #[getter]
    pub fn admin_group(&self) -> Option<&str> {
        self.0.admin_group.as_deref()
    }
}

#[pyclass]
pub struct Campaign(pub(crate) janitor::config::Campaign);

#[pymethods]
impl Campaign {
    #[getter]
    pub fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    #[getter]
    pub fn branch_name(&self) -> Option<&str> {
        self.0.branch_name.as_deref()
    }

    #[getter]
    pub fn merge_proposal(&self) -> Option<MergeProposalConfig> {
        self.0
            .merge_proposal
            .as_ref()
            .map(|m| MergeProposalConfig(m.clone()))
    }

    #[getter]
    pub fn force_build(&self) -> bool {
        self.0.force_build.unwrap_or(false)
    }

    #[getter]
    pub fn skip_setup_validation(&self) -> bool {
        self.0.skip_setup_validation.unwrap_or(false)
    }

    #[getter]
    pub fn default_empty(&self) -> bool {
        self.0.default_empty.unwrap_or(false)
    }

    #[getter]
    pub fn bugtracker(&self) -> Vec<BugTracker> {
        self.0
            .bugtracker
            .iter()
            .map(|b| BugTracker(b.clone()))
            .collect()
    }

    #[getter]
    pub fn debian_build(&self) -> Option<DebianBuild> {
        self.0.build.as_ref().and_then(|b| match b {
            janitor::config::config::campaign::Build::DebianBuild(d) => {
                Some(DebianBuild(d.clone()))
            }
            _ => None,
        })
    }

    #[getter]
    pub fn generic_build(&self) -> Option<GenericBuild> {
        self.0.build.as_ref().and_then(|b| match b {
            janitor::config::config::campaign::Build::GenericBuild(g) => {
                Some(GenericBuild(g.clone()))
            }
            _ => None,
        })
    }

    #[getter]
    pub fn build(&self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        match self.0.build.as_ref() {
            Some(janitor::config::config::campaign::Build::DebianBuild(d)) => {
                Ok(Some(Py::new(py, DebianBuild(d.clone()))?.into_any()))
            }
            Some(janitor::config::config::campaign::Build::GenericBuild(g)) => {
                Ok(Some(Py::new(py, GenericBuild(g.clone()))?.into_any()))
            }
            Some(_) => unreachable!(),
            None => Ok(None),
        }
    }

    #[getter]
    pub fn command(&self) -> Option<&str> {
        self.0.command.as_deref()
    }
}

#[pyclass]
pub struct DebianBuild(pub(crate) janitor::config::DebianBuild);

#[pymethods]
impl DebianBuild {
    #[getter]
    pub fn extra_build_distribution(&self) -> Vec<&str> {
        self.0
            .extra_build_distribution
            .iter()
            .map(|s| s.as_str())
            .collect()
    }

    #[getter]
    pub fn base_distribution(&self) -> Option<&str> {
        self.0.base_distribution.as_deref()
    }

    #[getter]
    pub fn chroot(&self) -> Option<&str> {
        self.0.chroot.as_deref()
    }

    #[getter]
    pub fn build_distribution(&self) -> Option<&str> {
        self.0.build_distribution.as_deref()
    }

    #[getter]
    pub fn build_suffix(&self) -> Option<&str> {
        self.0.build_suffix.as_deref()
    }

    #[getter]
    pub fn build_command(&self) -> Option<&str> {
        self.0.build_command.as_deref()
    }
}

#[pyclass]
pub struct GenericBuild(pub(crate) janitor::config::GenericBuild);

#[pymethods]
impl GenericBuild {
    #[getter]
    pub fn chroot(&self) -> Option<&str> {
        self.0.chroot.as_deref()
    }
}

#[pyclass]
pub struct MergeProposalConfig(pub(crate) janitor::config::MergeProposalConfig);

#[pymethods]
impl MergeProposalConfig {
    #[getter]
    pub fn value_threshold(&self) -> Option<i32> {
        self.0.value_threshold
    }

    #[getter]
    pub fn commit_message(&self) -> Option<&str> {
        self.0.commit_message.as_deref()
    }

    #[getter]
    pub fn title(&self) -> Option<&str> {
        self.0.title.as_deref()
    }

    #[getter]
    pub fn label(&self) -> Vec<&str> {
        self.0.label.iter().map(|s| s.as_str()).collect()
    }
}

#[pyclass]
pub struct Distribution(pub(crate) janitor::config::Distribution);

#[pymethods]
impl Distribution {
    #[getter]
    pub fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    #[getter]
    pub fn archive_mirror_uri(&self) -> Option<&str> {
        self.0.archive_mirror_uri.as_deref()
    }

    #[getter]
    pub fn signed_by(&self) -> Option<&str> {
        self.0.signed_by.as_deref()
    }

    #[getter]
    pub fn chroot(&self) -> Option<&str> {
        self.0.chroot.as_deref()
    }

    #[getter]
    pub fn chroot_alias(&self) -> Vec<&str> {
        self.0.chroot_alias.iter().map(|s| s.as_str()).collect()
    }

    #[getter]
    pub fn component(&self) -> Vec<&str> {
        self.0.component.iter().map(|s| s.as_str()).collect()
    }

    #[getter]
    pub fn lintian_profile(&self) -> Option<&str> {
        self.0.lintian_profile.as_deref()
    }

    #[getter]
    pub fn lintian_suppress_tag(&self) -> Vec<&str> {
        self.0
            .lintian_suppress_tag
            .iter()
            .map(|s| s.as_str())
            .collect()
    }

    #[getter]
    pub fn build_command(&self) -> Option<&str> {
        self.0.build_command.as_deref()
    }

    #[getter]
    pub fn vendor(&self) -> Option<&str> {
        self.0.vendor.as_deref()
    }

    #[getter]
    pub fn extra(&self) -> Vec<&str> {
        self.0.extra.iter().map(|s| s.as_str()).collect()
    }
}

#[pyclass]
pub struct AptRepository(pub(crate) janitor::config::AptRepository);

#[pymethods]
impl AptRepository {
    #[getter]
    pub fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    #[getter]
    pub fn base(&self) -> Option<&str> {
        self.0.base.as_deref()
    }

    #[getter]
    pub fn select(&self) -> Vec<Select> {
        self.0.select.iter().map(|s| Select(s.clone())).collect()
    }

    #[getter]
    pub fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }
}

#[pyclass]
pub struct Select(pub(crate) janitor::config::Select);

#[pymethods]
impl Select {
    #[getter]
    pub fn campaign(&self) -> Option<&str> {
        self.0.campaign.as_deref()
    }
}

#[pyfunction]
pub fn read_string(s: &str) -> PyResult<Config> {
    let config =
        janitor::config::read_string(s).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(Config(config))
}

#[pyfunction]
pub fn read_config(f: Py<PyAny>) -> PyResult<Config> {
    let f = pyo3_filelike::PyTextFile::from(f);
    let config =
        janitor::config::read_readable(f).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(Config(config))
}

#[pyfunction]
pub fn get_campaign_config(config: &Config, campaign: &str) -> PyResult<Campaign> {
    config.find_campaign(campaign)
}

#[pyfunction]
pub fn get_distribution(config: &Config, distribution: &str) -> PyResult<Distribution> {
    config.find_distribution(distribution)
}

pub(crate) fn init(_py: Python, module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<Config>()?;
    module.add_class::<Campaign>()?;
    module.add_class::<AptRepository>()?;
    module.add_class::<Distribution>()?;
    module.add_class::<BugTracker>()?;
    module.add_class::<OAuth2Provider>()?;
    module.add_class::<MergeProposalConfig>()?;
    module.add_class::<DebianBuild>()?;
    module.add_class::<GenericBuild>()?;
    module.add_class::<Select>()?;
    module.add_function(wrap_pyfunction!(read_config, module)?)?;
    module.add_function(wrap_pyfunction!(get_campaign_config, module)?)?;
    module.add_function(wrap_pyfunction!(get_distribution, module)?)?;
    module.add_function(wrap_pyfunction!(read_string, module)?)?;
    Ok(())
}
